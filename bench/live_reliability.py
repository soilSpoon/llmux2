#!/usr/bin/env python3
"""Live reliability suite: drive llmux-server through faults in front of real providers.

Every scenario starts a fresh gateway with a generated config whose model has a
faulted primary deployment and a healthy fallback. Upstream traffic goes
through an in-process fault proxy (or straight to a dead address for
network-layer outages), so the suite can count which upstream was contacted
and inject 429s, quota errors, hangs, resets and mid-stream cuts while the
healthy path still reaches the real provider. ``--offline`` swaps the real
providers for a local fake upstream so the suite itself can be tested.

Provider keys never leave the files named by the source config: the gateway
reads them, and response bodies are redacted before they are reported.
"""

import argparse
import contextlib
import http.client
import http.server
import json
import os
import re
import secrets
import socket
import socketserver
import ssl
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse

HOP_HEADERS = {
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "host",
    "content-length",
    "accept-encoding",
}
PROMPT = "Reply with the single word OK."
MAX_BODY = 4096


## config


def parse_toml_subset(text):
    """Parse the TOML subset llmux-import writes: tables, arrays of tables,
    strings, integers, booleans and string arrays."""
    root = {}
    current = root
    for number, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        header = re.fullmatch(r"(\[\[?)\s*([A-Za-z0-9_.]+)\s*(\]\]?)", line)
        if header:
            many = header.group(1) == "[["
            path = header.group(2).split(".")
            node = root
            for part in path[:-1]:
                node = node.setdefault(part, {})
                if isinstance(node, list):
                    node = node[-1]
            if many:
                node = node.setdefault(path[-1], [])
                node.append({})
                current = node[-1]
            else:
                current = node.setdefault(path[-1], {})
            continue
        key, sep, value = line.partition("=")
        if not sep:
            raise ValueError("line %d: expected key = value" % number)
        current[key.strip().strip('"')] = _toml_value(value.strip(), number)
    return root


def _toml_value(value, number):
    if value.startswith("["):
        inner = value[1:-1].strip()
        if not inner:
            return []
        return [_toml_value(item.strip(), number) for item in _split_array(inner)]
    if value.startswith('"'):
        return json.loads(value)
    if value in ("true", "false"):
        return value == "true"
    if re.fullmatch(r"-?\d+", value):
        return int(value)
    raise ValueError("line %d: unsupported value %r" % (number, value))


def _split_array(inner):
    items, depth, quoted, start, escaped = [], 0, False, 0, False
    for i, ch in enumerate(inner):
        if quoted:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                quoted = False
        elif ch == '"':
            quoted = True
        elif ch == "[":
            depth += 1
        elif ch == "]":
            depth -= 1
        elif ch == "," and depth == 0:
            items.append(inner[start:i])
            start = i + 1
    tail = inner[start:].strip()
    if tail:
        items.append(tail)
    return items


def toml_str(value):
    return json.dumps(value)


def render_config(cfg):
    """Render the generated scenario config (dict) as TOML."""
    out = []
    for section in ("server", "routing", "cooldown"):
        if section in cfg:
            out.append("[%s]" % section)
            for key, value in cfg[section].items():
                out.append("%s = %s" % (key, _render_value(value)))
            out.append("")
    for credential in cfg["credentials"]:
        out.append("[[credentials]]")
        for key, value in credential.items():
            out.append("%s = %s" % (key, _render_value(value)))
        out.append("")
    for model in cfg["models"]:
        out.append("[[models]]")
        out.append("name = %s" % toml_str(model["name"]))
        out.append("")
        for dep in model["deployments"]:
            quota = dep.get("quota")
            out.append("[[models.deployments]]")
            for key, value in dep.items():
                if key != "quota":
                    out.append("%s = %s" % (key, _render_value(value)))
            if quota:
                out.append("[models.deployments.quota]")
                for key, value in quota.items():
                    out.append("%s = %s" % (key, _render_value(value)))
            out.append("")
    return "\n".join(out)


def _render_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, list):
        return "[" + ", ".join(_render_value(v) for v in value) + "]"
    return toml_str(value)


class Target:
    """One real upstream (base URL + credential + model) the suite can exercise."""

    def __init__(self, name, provider, base_url, key_ref, upstream_model, kind="api_key", extra=None):
        self.name = name
        self.provider = provider
        self.base_url = base_url.rstrip("/")
        self.key_ref = key_ref
        self.upstream_model = upstream_model
        self.kind = kind
        self.extra = extra or {}

    def describe(self):
        host = urllib.parse.urlsplit(self.base_url).hostname
        return "%s (%s, %s)" % (host, self.provider, self.upstream_model)


def targets_from_config(cfg, only_hosts=None):
    """One target per distinct upstream base URL, using the first deployment that names it."""
    credentials = {c["id"]: c for c in cfg.get("credentials", [])}
    seen = {}
    for model in cfg.get("models", []):
        for dep in model.get("deployments", []):
            credential = credentials.get(dep.get("credential"))
            if not credential or credential.get("kind") != "api_key":
                continue
            base = dep["base_url"].rstrip("/")
            host = urllib.parse.urlsplit(base).hostname
            if only_hosts and host not in only_hosts:
                continue
            if base in seen:
                continue
            seen[base] = Target(
                name=re.sub(r"[^A-Za-z0-9]+", "-", host).strip("-"),
                provider=credential["provider"],
                base_url=base,
                key_ref=credential["key"],
                upstream_model=dep["upstream_model"],
            )
    return list(seen.values())


def read_secret(reference):
    if reference.startswith("file:"):
        with open(reference[5:]) as handle:
            return handle.read().strip()
    if reference.startswith("env:"):
        return os.environ[reference[4:]]
    raise ValueError("secret references must be env: or file:")


## fault proxy


class Route:
    """Per-deployment behaviour behind the proxy. ``fault`` may change between requests."""

    def __init__(self, upstream, fault="pass", **options):
        self.upstream = upstream.rstrip("/")
        self.fault = fault
        self.options = options
        self.hits = 0
        self.statuses = []
        self.lock = threading.Lock()

    def record(self, status):
        with self.lock:
            self.statuses.append(status)


class FaultProxy(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), _ProxyHandler)
        self.routes = {}
        self.auth = None  # OAuthServer whose tokens the "oauth" fault accepts
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)
        self.thread.start()

    @property
    def port(self):
        return self.server_address[1]

    def add(self, route_id, route):
        self.routes[route_id] = route
        return "http://127.0.0.1:%d/r/%s" % (self.port, route_id)

    def close(self):
        self.shutdown()
        self.server_close()


def _abort(sock):
    """Close with RST so the gateway sees a broken connection, not a clean EOF."""
    with contextlib.suppress(OSError):
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
    with contextlib.suppress(OSError):
        sock.close()


class _ProxyHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        self._handle()

    def do_GET(self):
        self._handle()

    def _handle(self):
        match = re.match(r"^/r/([^/]+)(/.*)$", self.path)
        route = match and self.server.routes.get(match.group(1))
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length) if length else b""
        if not route:
            self._reply(404, {"error": {"message": "unknown route"}})
            return
        with route.lock:
            route.hits += 1
        fault, opts = route.fault, route.options
        if fault == "status":
            headers = dict(opts.get("headers", {}))
            route.record(opts["status"])
            self._reply(opts["status"], opts.get("body", {"error": {"message": "injected"}}), headers)
        elif fault == "hang":
            route.record("hang")
            time.sleep(opts.get("seconds", 120))
            _abort(self.connection)
            self.close_connection = True
        elif fault == "reset":
            route.record("reset")
            _abort(self.connection)
            self.close_connection = True
        elif fault == "sse_then_close":
            route.record(200)
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("transfer-encoding", "chunked")
            self.end_headers()
            self.wfile.flush()
            time.sleep(0.2)
            _abort(self.connection)
            self.close_connection = True
        else:
            self._forward(route, match.group(2), body)

    def _reply(self, status, payload, headers=None):
        data = json.dumps(payload).encode()
        self.send_response(status)
        for key, value in (headers or {}).items():
            self.send_header(key, value)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _forward(self, route, suffix, body):
        opts = route.options
        headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP_HEADERS}
        if route.fault == "oauth":
            token = (self.headers.get("authorization") or "").partition(" ")[2]
            if not self.server.auth.valid(token):
                route.record(401)
                self._reply(401, {"error": {"message": "token expired or revoked", "type": "invalid_request_error", "code": "invalid_api_key"}})
                return
            for key in list(headers):
                if key.lower() in ("authorization", "x-api-key"):
                    del headers[key]
            headers["Authorization"] = "Bearer " + opts["real_key"]
        url = urllib.parse.urlsplit(route.upstream)
        path = url.path + suffix
        connection_class = http.client.HTTPSConnection if url.scheme == "https" else http.client.HTTPConnection
        kwargs = {"timeout": opts.get("upstream_timeout", 120)}
        if url.scheme == "https":
            kwargs["context"] = ssl.create_default_context()
        upstream = connection_class(url.hostname, url.port, **kwargs)
        try:
            headers["Host"] = url.netloc
            headers["Accept-Encoding"] = "identity"
            headers["Content-Length"] = str(len(body))
            upstream.request(self.command, path, body=body, headers=headers)
            response = upstream.getresponse()
        except (OSError, http.client.HTTPException):
            route.record("upstream-error")
            _abort(self.connection)
            self.close_connection = True
            return
        route.record(response.status)
        ctype = response.getheader("content-type", "")
        self.send_response(response.status)
        for key, value in response.getheaders():
            if key.lower() not in HOP_HEADERS:
                self.send_header(key, value)
        if "text/event-stream" not in ctype:
            data = response.read()
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            upstream.close()
            return
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()
        cut_after = opts.get("cut_after_events") if route.fault in ("cut", "stall") else None
        events = 0
        try:
            while True:
                line = response.readline()
                if not line:
                    break
                self._chunk(line)
                if line in (b"\n", b"\r\n"):
                    events += 1
                    if cut_after is not None and events >= cut_after:
                        if route.fault == "stall":
                            time.sleep(opts.get("seconds", 120))
                        _abort(self.connection)
                        self.close_connection = True
                        return
            self.wfile.write(b"0\r\n\r\n")
        except OSError:
            self.close_connection = True
        finally:
            upstream.close()

    def _chunk(self, data):
        self.wfile.write(b"%x\r\n%s\r\n" % (len(data), data))
        self.wfile.flush()


## oauth token server


class OAuthServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    """Loopback refresh-token grant endpoint. Tokens are random and can be revoked."""

    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), _OAuthHandler)
        self.lock = threading.Lock()
        self.tokens = set()
        self.refresh_token = secrets.token_urlsafe(16)
        self.fail_refresh = False
        self.grants = 0
        threading.Thread(target=self.serve_forever, daemon=True).start()

    @property
    def url(self):
        return "http://127.0.0.1:%d/token" % self.server_address[1]

    def valid(self, token):
        with self.lock:
            return token in self.tokens

    def revoke_all(self):
        with self.lock:
            self.tokens.clear()

    def close(self):
        self.shutdown()
        self.server_close()


class _OAuthHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        form = urllib.parse.parse_qs(self.rfile.read(length).decode())
        server = self.server
        with server.lock:
            ok = (
                not server.fail_refresh
                and form.get("grant_type") == ["refresh_token"]
                and form.get("refresh_token") == [server.refresh_token]
            )
            if ok:
                token = secrets.token_urlsafe(24)
                server.tokens.add(token)
                server.grants += 1
        if ok:
            payload, status = {"access_token": token, "token_type": "Bearer", "expires_in": 3600}, 200
        else:
            payload, status = {"error": "invalid_grant"}, 400
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


## fake upstream (offline mode)


class FakeUpstream(socketserver.ThreadingMixIn, http.server.HTTPServer):
    """Minimal OpenAI-compatible upstream used by --offline and the unit tests."""

    daemon_threads = True

    def __init__(self, key="offline-key"):
        super().__init__(("127.0.0.1", 0), _FakeHandler)
        self.key = key
        self.hits = 0
        threading.Thread(target=self.serve_forever, daemon=True).start()

    @property
    def url(self):
        return "http://127.0.0.1:%d/v1" % self.server_address[1]

    def close(self):
        self.shutdown()
        self.server_close()


class _FakeHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        self.server.hits += 1
        length = int(self.headers.get("content-length") or 0)
        request = json.loads(self.rfile.read(length) or b"{}")
        if self.headers.get("authorization") != "Bearer " + self.server.key:
            data = b'{"error":{"message":"Incorrect API key","type":"invalid_request_error","code":"invalid_api_key"}}'
            self.send_response(401)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        model = request.get("model", "fake")
        if request.get("stream"):
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", "")
            self.end_headers()
            parts = []
            for i, word in enumerate(["O", "K", ".", "", ""]):
                chunk = {"id": "c", "object": "chat.completion.chunk", "model": model, "choices": [{"index": 0, "delta": {"content": word} if i < 3 else {}, "finish_reason": "stop" if i == 4 else None}]}
                parts.append(b"data: " + json.dumps(chunk).encode() + b"\n\n")
            parts.append(b"data: [DONE]\n\n")
            for part in parts:
                self.wfile.write(part)
                self.wfile.flush()
                time.sleep(0.05)
            self.close_connection = True
            return
        data = json.dumps({"id": "c", "object": "chat.completion", "model": model, "choices": [{"index": 0, "message": {"role": "assistant", "content": "OK"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def send_header(self, key, value):
        if key == "content-length" and value == "":
            return
        super().send_header(key, value)


## gateway


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Gateway:
    def __init__(self, binary, workdir, cfg):
        self.port = free_port()
        self.key = secrets.token_urlsafe(24)
        client_key = os.path.join(workdir, "client.key")
        with open(client_key, "w") as handle:
            handle.write(self.key)
        cfg = dict(cfg)
        cfg["server"] = dict(cfg.get("server", {}), listen="127.0.0.1:%d" % self.port, client_keys=["file:" + client_key])
        self.config_path = os.path.join(workdir, "llmux.toml")
        with open(self.config_path, "w") as handle:
            handle.write(render_config(cfg))
        self.log = open(os.path.join(workdir, "server.log"), "w")
        self.proc = subprocess.Popen([binary, self.config_path], stdout=self.log, stderr=subprocess.STDOUT)
        deadline = time.time() + 20
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError("gateway exited: " + open(self.log.name).read()[-500:])
            with contextlib.suppress(OSError), socket.create_connection(("127.0.0.1", self.port), 0.2):
                return
            time.sleep(0.05)
        raise RuntimeError("gateway did not start")

    def request(self, model, stream=False, route="chat", timeout=150):
        if route == "responses":
            path, payload = "/v1/responses", {"model": model, "input": PROMPT, "max_output_tokens": 16, "stream": stream}
        else:
            path, payload = "/v1/chat/completions", {"model": model, "messages": [{"role": "user", "content": PROMPT}], "max_tokens": 16, "stream": stream}
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)
        start = time.time()
        try:
            try:
                connection.request("POST", path, body=json.dumps(payload), headers={"Authorization": "Bearer " + self.key, "Content-Type": "application/json"})
                response = connection.getresponse()
            except (OSError, http.client.HTTPException) as exc:
                return Result(0, {}, b"", time.time() - start, stream, type(exc).__name__)
            body = b""
            error = None
            try:
                body = response.read()
            except (OSError, http.client.HTTPException) as exc:
                error = type(exc).__name__
            return Result(response.status, dict((k.lower(), v) for k, v in response.getheaders()), body, time.time() - start, stream, error)
        finally:
            connection.close()

    def close(self):
        self.proc.terminate()
        with contextlib.suppress(subprocess.TimeoutExpired):
            self.proc.wait(5)
        if self.proc.poll() is None:
            self.proc.kill()
        self.log.close()


REDACT = str


class Result:
    def __init__(self, status, headers, body, seconds, stream, transport_error=None):
        self.status = status
        self.headers = headers
        self.body = REDACT(body.decode("utf-8", "replace"))
        self.seconds = seconds
        self.stream = stream
        self.transport_error = transport_error

    @property
    def deployment(self):
        return self.headers.get("x-llmux-deployment")

    @property
    def attempts(self):
        value = self.headers.get("x-llmux-attempts")
        return int(value) if value and value.isdigit() else None

    @property
    def events(self):
        return [line[5:].strip() for line in self.body.splitlines() if line.startswith("data:")]

    @property
    def stream_done(self):
        return "[DONE]" in self.events or "response.completed" in self.body

    @property
    def stream_error(self):
        return "stream_broken" in self.body

    @property
    def content_events(self):
        return sum(1 for e in self.events if e != "[DONE]" and "stream_broken" not in e)

    def summary(self):
        parts = ["%d" % self.status, "dep=%s" % (self.deployment or "-"), "attempts=%s" % (self.attempts or "-"), "%.1fs" % self.seconds]
        if self.stream:
            parts.append("events=%d done=%s broken=%s" % (self.content_events, self.stream_done, self.stream_error))
        if self.status != 200:
            parts.append(re.sub(r"\s+", " ", self.body[:160]))
        return " ".join(parts)


## scenarios


class Suite:
    def __init__(self, binary, primary, fallback, proxy, workroot, redact, request_timeout_ms=60000):
        self.binary = binary
        self.primary = primary
        self.fallback = fallback
        self.proxy = proxy
        self.workroot = workroot
        self.redact = redact
        self.request_timeout_ms = request_timeout_ms
        self.results = []
        self.counter = 0

    # deployment helpers -------------------------------------------------------

    def _route(self, target, fault="pass", **options):
        self.counter += 1
        route = Route(target.base_url, fault, **options)
        url = self.proxy.add("%d-%s" % (self.counter, target.name), route)
        return url, route

    def deployment(self, dep_id, target, priority, base_url=None, key_ref=None, credential=None, **extra):
        cred_id = credential or "cred-" + dep_id
        dep = {"id": dep_id, "credential": cred_id, "base_url": base_url or target.base_url, "upstream_model": target.upstream_model, "priority": priority}
        dep.update(extra)
        cred = None if credential else {"id": cred_id, "kind": "api_key", "provider": target.provider, "key": key_ref or target.key_ref}
        return dep, cred

    def run_gateway(self, name, deployments, credentials, routing=None, cooldown=None, models=None):
        workdir = os.path.join(self.workroot, name)
        os.makedirs(workdir, mode=0o700, exist_ok=True)
        cfg = {
            "server": {"request_timeout_ms": self.request_timeout_ms},
            "routing": routing or {"max_attempts": 3},
            "cooldown": cooldown or {"default_ms": 5000, "max_ms": 60000},
            "credentials": [c for c in credentials if c],
            "models": models or [{"name": "rel", "deployments": deployments}],
        }
        return Gateway(self.binary, workdir, cfg)

    def bad_key_file(self, name):
        path = os.path.join(self.workroot, name + ".badkey")
        with open(path, "w") as handle:
            handle.write("sk-llmux-reliability-invalid-" + secrets.token_hex(12))
        return "file:" + path

    def check(self, scenario, expectation, ok, observed, why=""):
        verdict = "PASS" if ok else "FAIL"
        record = {"scenario": scenario, "expect": expectation, "verdict": verdict, "observed": self.redact(observed), "note": why}
        self.results.append(record)
        print("%-4s %-34s %s" % (verdict, scenario, record["observed"]), flush=True)
        return ok

    def skip(self, scenario, expectation, why):
        self.results.append({"scenario": scenario, "expect": expectation, "verdict": "SKIP", "observed": "", "note": why})
        print("SKIP %-34s %s" % (scenario, why), flush=True)

    def pair(self, name, fault, stream=False, primary_extra=None, routing=None, primary_options=None):
        """Faulted primary (priority 0) + proxied healthy fallback (priority 1)."""
        url, proute = self._route(self.primary, fault, **(primary_options or {}))
        furl, froute = self._route(self.fallback)
        p, pc = self.deployment("primary", self.primary, 0, base_url=url, **(primary_extra or {}))
        f, fc = self.deployment("fallback", self.fallback, 1, base_url=furl)
        return self.run_gateway(name, [p, f], [pc, fc], routing=routing), proute, froute

    # scenarios -----------------------------------------------------------------

    def failover(self, name, fault, expectation, stream=False, primary_options=None, primary_extra=None, route="chat"):
        gw, proute, froute = self.pair(name, fault, stream, primary_extra=primary_extra, primary_options=primary_options)
        try:
            r = gw.request("rel", stream=stream, route=route)
            ok = r.status == 200 and r.deployment == "fallback" and proute.hits >= 1 and froute.hits == 1
            if stream:
                ok = ok and r.stream_done and not r.stream_error
            return self.check(name, expectation, ok, r.summary() + " primary_upstream=%s fallback_upstream=%s" % (proute.statuses, froute.statuses)), gw, proute, froute
        except Exception:
            gw.close()
            raise

    def run_all(self, oauth_key=None):
        s = self
        # Rate limits and cooldown -------------------------------------------------
        ok, gw, proute, froute = s.failover("rate_limit_429_failover", "status", "429 + Retry-After on primary -> served by fallback, attempts 2", primary_options={"status": 429, "headers": {"Retry-After": "4"}, "body": {"error": {"message": "Rate limit reached for requests", "type": "requests", "code": "rate_limit_exceeded"}}})
        try:
            hits = proute.hits
            r = gw.request("rel")
            s.check("rate_limit_cooldown_skips_primary", "next request inside Retry-After skips primary without contacting it", r.status == 200 and r.deployment == "fallback" and r.attempts == 1 and proute.hits == hits, r.summary() + " primary_hits=%d" % (proute.hits - hits))
            proute.fault = "pass"
            time.sleep(4.5)
            r = gw.request("rel")
            s.check("rate_limit_cooldown_expires", "after Retry-After the primary is used again", r.status == 200 and r.deployment == "primary", r.summary())
        finally:
            gw.close()

        for name, status, body in (
            ("quota_429_insufficient_quota", 429, {"error": {"message": "You exceeded your current quota", "type": "insufficient_quota", "code": "insufficient_quota"}}),
            ("quota_402_payment_required", 402, {"error": {"message": "Insufficient credits", "code": 402}}),
        ):
            ok, gw, proute, froute = s.failover(name, "status", "quota error on primary -> fallback, primary cooled down", primary_options={"status": status, "body": body})
            try:
                hits = proute.hits
                r = gw.request("rel")
                s.check(name + "_cooldown", "quota cooldown keeps primary out of rotation", r.status == 200 and r.deployment == "fallback" and proute.hits == hits, r.summary())
            finally:
                gw.close()

        url, proute = s._route(s.primary)
        furl, froute = s._route(s.fallback)
        p, pc = s.deployment("primary", s.primary, 0, base_url=url, quota={"rpm": 1})
        f, fc = s.deployment("fallback", s.fallback, 1, base_url=furl)
        gw = s.run_gateway("local_rpm_limit", [p, f], [pc, fc])
        try:
            first = gw.request("rel")
            second = gw.request("rel")
            s.check("local_rpm_limit", "rpm=1: second request goes to fallback without contacting primary", first.deployment == "primary" and second.deployment == "fallback" and second.attempts == 1 and proute.hits == 1, first.summary() + " | " + second.summary())
        finally:
            gw.close()

        url, proute = s._route(s.primary, "status", status=429, headers={"Retry-After": "7"})
        furl, froute = s._route(s.fallback, "status", status=429, headers={"Retry-After": "3"})
        p, pc = s.deployment("primary", s.primary, 0, base_url=url)
        f, fc = s.deployment("fallback", s.fallback, 1, base_url=furl)
        gw = s.run_gateway("all_rate_limited", [p, f], [pc, fc])
        try:
            r = gw.request("rel")
            retry = r.headers.get("retry-after", "")
            s.check("all_rate_limited", "every deployment 429 -> gateway 429 with earliest Retry-After (3s)", r.status == 429 and retry.isdigit() and 1 <= int(retry) <= 3, r.summary() + " retry-after=%s" % retry)
        finally:
            gw.close()

        url, proute = s._route(s.primary, "status", status=429, headers={"Retry-After": "6"})
        furl, froute = s._route(s.fallback)
        pa, pc = s.deployment("a-primary", s.primary, 0, base_url=url)
        pb, _ = s.deployment("b-primary", s.primary, 0, base_url=url, credential=pa["credential"])
        fa, fc = s.deployment("a-fallback", s.fallback, 1, base_url=furl)
        fb, _ = s.deployment("b-fallback", s.fallback, 1, base_url=furl, credential=fa["credential"])
        models = [{"name": "rel-a", "deployments": [pa, fa]}, {"name": "rel-b", "deployments": [pb, fb]}]
        gw = s.run_gateway("shared_route_cooldown", None, [pc, fc], models=models)
        try:
            first = gw.request("rel-a")
            second = gw.request("rel-b")
            s.check("shared_route_cooldown", "429 on model A's route -> model B, which lists the same provider/key/model, skips that route", first.deployment == "a-fallback" and second.deployment == "b-fallback" and second.attempts == 1 and proute.hits == 1, first.summary() + " | " + second.summary() + " primary_hits=%d" % proute.hits)
        finally:
            gw.close()

        # Bad credentials ------------------------------------------------------------
        s.bad_key(s.primary, "bad_key_primary")

        # Server errors, timeouts and network-layer outages --------------------------
        for name, fault, opts, extra, expectation in (
            ("server_503_failover", "status", {"status": 503}, {}, "503 on primary -> fallback"),
            ("timeout_hang_failover", "hang", {"seconds": 30}, {"timeout_ms": 3000}, "primary accepts but never answers -> fallback after the 3s deployment timeout"),
            ("outage_tcp_reset", "reset", {}, {}, "primary resets the connection -> fallback"),
        ):
            ok, gw, proute, froute = s.failover(name, fault, expectation, primary_options=opts, primary_extra=extra)
            gw.close()
        for name, base, extra, expectation in (
            ("outage_connection_refused", "http://127.0.0.1:%d/v1" % free_port(), {}, "nothing listens on primary -> fallback"),
            ("outage_dns_failure", "https://llmux-outage.invalid/v1", {}, "primary host does not resolve -> fallback"),
            ("outage_blackhole", "https://10.255.255.1/v1", {"timeout_ms": 3000}, "primary packets dropped -> fallback after the 3s deployment timeout"),
        ):
            furl, froute = s._route(s.fallback)
            p, pc = s.deployment("primary", s.primary, 0, base_url=base, **extra)
            f, fc = s.deployment("fallback", s.fallback, 1, base_url=furl)
            gw = s.run_gateway(name, [p, f], [pc, fc])
            try:
                r = gw.request("rel")
                s.check(name, expectation, r.status == 200 and r.deployment == "fallback" and r.attempts == 2 and r.seconds < 30, r.summary())
            finally:
                gw.close()

        # Streaming ---------------------------------------------------------------------
        ok, gw, proute, froute = s.failover("stream_429_failover", "status", "stream: 429 on primary -> fallback stream completes", stream=True, primary_options={"status": 429, "headers": {"Retry-After": "5"}})
        gw.close()
        ok, gw, proute, froute = s.failover("stream_setup_timeout_failover", "hang", "stream: primary never sends headers -> fallback after 3s", stream=True, primary_options={"seconds": 30}, primary_extra={"timeout_ms": 3000})
        gw.close()
        ok, gw, proute, froute = s.failover("stream_close_before_first_event", "sse_then_close", "stream: primary sends 200 then drops before any event (uncommitted) -> fallback", stream=True)
        gw.close()
        gw, proute, froute = s.pair("stream_midstream_cut", "cut", primary_options={"cut_after_events": 2})
        try:
            r = gw.request("rel", stream=True)
            s.check("stream_midstream_cut", "stream: primary cut after 2 events -> committed stream ends with stream_broken error, no replay on fallback", r.status == 200 and r.deployment == "primary" and r.stream_error and not r.stream_done and froute.hits == 0 and r.content_events >= 1, r.summary() + " fallback_hits=%d" % froute.hits)
        finally:
            gw.close()
        gw, proute, froute = s.pair("stream_idle_stall", "stall", primary_options={"cut_after_events": 2, "seconds": 30}, primary_extra={"timeout_ms": 4000})
        try:
            r = gw.request("rel", stream=True)
            s.check("stream_idle_stall", "stream: primary stalls after 2 events -> stream_broken after the 4s idle timeout, no replay", r.status == 200 and r.deployment == "primary" and r.stream_error and froute.hits == 0 and r.seconds < 20, r.summary() + " fallback_hits=%d" % froute.hits)
        finally:
            gw.close()
        if s.fallback.provider == "anthropic":
            s.skip("responses_429_failover", "Responses API: 429 on primary -> fallback", "Anthropic fallback cannot serve the Responses API")
        else:
            ok, gw, proute, froute = s.failover("responses_429_failover", "status", "Responses API: 429 on primary -> fallback", route="responses", primary_options={"status": 429, "headers": {"Retry-After": "5"}})
            gw.close()

        # OAuth refresh -------------------------------------------------------------------
        if s.primary.provider not in ("openai", "openai_compat") or oauth_key is None:
            s.skip("oauth_refresh_on_401", "revoked access token -> one refresh -> retry succeeds on the same deployment", "primary is not OpenAI-compatible")
        else:
            s.oauth(oauth_key)
        return s.results

    def bad_key(self, target, name):
        s = self
        url, proute = s._route(target)
        furl, froute = s._route(s.fallback)
        p, pc = s.deployment("primary", target, 0, base_url=url, key_ref=s.bad_key_file(name))
        f, fc = s.deployment("fallback", s.fallback, 1, base_url=furl)
        gw = s.run_gateway(name, [p, f], [pc, fc])
        try:
            r = gw.request("rel")
            upstream = proute.statuses[:]
            s.check(name + ":" + target.name, "invalid key at %s -> fallback" % target.describe(), r.status == 200 and r.deployment == "fallback" and froute.hits == 1, r.summary() + " primary_upstream=%s" % upstream)
            hits = proute.hits
            r = gw.request("rel")
            s.check(name + "_disabled:" + target.name, "rejected credential is not retried on the next request", r.status == 200 and r.deployment == "fallback" and proute.hits == hits, r.summary() + " primary_hits=%d" % (proute.hits - hits))
        finally:
            gw.close()

    def oauth(self, real_key):
        s = self
        auth = OAuthServer()
        s.proxy.auth = auth
        refresh_path = os.path.join(s.workroot, "oauth.refresh")
        with open(refresh_path, "w") as handle:
            handle.write(auth.refresh_token)
        url, proute = s._route(s.primary, "oauth", real_key=real_key)
        furl, froute = s._route(s.fallback)
        p, _ = s.deployment("primary", s.primary, 0, base_url=url, credential="oauth-primary")
        f, fc = s.deployment("fallback", s.fallback, 1, base_url=furl)
        oc = {"id": "oauth-primary", "kind": "oauth", "provider": "openai_compat", "token_url": auth.url, "client_id": "llmux-reliability", "refresh_token": "file:" + refresh_path}
        gw = s.run_gateway("oauth_refresh", [p, f], [oc, fc])
        try:
            r = gw.request("rel")
            s.check("oauth_initial_token", "startup token is accepted by the real provider", r.status == 200 and r.deployment == "primary", r.summary())
            auth.revoke_all()
            grants = auth.grants
            r = gw.request("rel")
            s.check("oauth_refresh_on_401", "revoked token -> one refresh -> same deployment succeeds (attempts 2)", r.status == 200 and r.deployment == "primary" and r.attempts == 2 and auth.grants == grants + 1, r.summary() + " refreshes=%d" % (auth.grants - grants))
            auth.revoke_all()
            auth.fail_refresh = True
            r = gw.request("rel")
            s.check("oauth_refresh_failure_failover", "revoked token and refresh rejected -> fallback", r.status == 200 and r.deployment == "fallback", r.summary())
            hits = proute.hits
            r = gw.request("rel")
            s.check("oauth_disabled_after_failure", "credential with failed refresh is skipped next time", r.status == 200 and r.deployment == "fallback" and proute.hits == hits, r.summary())
        finally:
            gw.close()
            auth.close()


## probes and report


def probe(binary, proxy, workroot, targets, redact):
    """Send one tiny real request to each target through the gateway; report health."""
    rows = []
    for target in targets:
        route = Route(target.base_url)
        url = proxy.add("probe-" + target.name, route)
        workdir = os.path.join(workroot, "probe-" + target.name)
        os.makedirs(workdir, mode=0o700, exist_ok=True)
        cfg = {
            "server": {"request_timeout_ms": 60000},
            "routing": {"max_attempts": 1},
            "credentials": [{"id": "c", "kind": "api_key", "provider": target.provider, "key": target.key_ref}],
            "models": [{"name": "rel", "deployments": [{"id": "d", "credential": "c", "base_url": url, "upstream_model": target.upstream_model}]}],
        }
        gw = Gateway(binary, workdir, cfg)
        try:
            r = gw.request("rel")
        finally:
            gw.close()
        rows.append({"target": target.describe(), "healthy": r.status == 200, "observed": redact(r.summary()), "upstream": route.statuses})
        print("PROBE %-60s %s" % (target.describe(), rows[-1]["observed"]), flush=True)
    return rows


def markdown(report):
    lines = ["# llmux2 live reliability run", "", "- primary: %s" % report["primary"], "- fallback: %s" % report["fallback"], "- binary: %s" % report["binary"], ""]
    if report.get("probes"):
        lines += ["## Provider probes", "", "| target | healthy | observed | upstream status |", "| --- | --- | --- | --- |"]
        for row in report["probes"]:
            lines.append("| %s | %s | %s | %s |" % (row["target"], "yes" if row["healthy"] else "no", row["observed"].replace("|", "/"), row["upstream"]))
        lines.append("")
    counts = {}
    for row in report["results"]:
        counts[row["verdict"]] = counts.get(row["verdict"], 0) + 1
    lines += ["## Scenarios (%s)" % ", ".join("%d %s" % (v, k) for k, v in sorted(counts.items())), "", "| verdict | scenario | expected | observed |", "| --- | --- | --- | --- |"]
    for row in report["results"]:
        lines.append("| %s | %s | %s | %s |" % (row["verdict"], row["scenario"], row["expect"], (row["observed"] or row["note"]).replace("|", "/")))
    return "\n".join(lines) + "\n"


def make_redactor(values):
    values = [v for v in values if v and len(v) >= 8]

    def redact(text):
        for value in values:
            text = text.replace(value, "[REDACTED]")
        return re.sub(r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]{8,}", r"\1[REDACTED]", text)

    return redact


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--binary", default="target/release/llmux-server")
    parser.add_argument("--config", help="source llmux.toml whose real providers are exercised")
    parser.add_argument("--primary", help="host of the provider to fault (default: first healthy openai-compatible target)")
    parser.add_argument("--fallback", help="host of the healthy fallback (default: first other healthy target)")
    parser.add_argument("--bad-key-hosts", default="all", help="comma-separated hosts for the invalid-key sweep, 'all' or 'none'")
    parser.add_argument("--offline", action="store_true", help="use local fake upstreams instead of real providers")
    parser.add_argument("--out", default="live-reliability", help="directory for report.md and report.json")
    args = parser.parse_args(argv)

    old_umask = os.umask(0o077)
    proxy = FaultProxy()
    fakes = []
    try:
        with tempfile.TemporaryDirectory(prefix="llmux-reliability-") as workroot:
            secrets_seen = []
            if args.offline:
                fakes = [FakeUpstream(), FakeUpstream()]
                key_path = os.path.join(workroot, "offline.key")
                with open(key_path, "w") as handle:
                    handle.write("offline-key")
                targets = [Target("fake-%d" % i, "openai_compat", fake.url, "file:" + key_path, "fake-model") for i, fake in enumerate(fakes)]
            else:
                if not args.config:
                    parser.error("--config is required unless --offline")
                with open(args.config) as handle:
                    cfg = parse_toml_subset(handle.read())
                targets = targets_from_config(cfg)
                for target in targets:
                    with contextlib.suppress(OSError, KeyError, ValueError):
                        secrets_seen.append(read_secret(target.key_ref))
            redact = make_redactor(secrets_seen)
            global REDACT
            REDACT = redact
            probes = probe(args.binary, proxy, workroot, targets, redact)
            healthy = [t for t, row in zip(targets, probes) if row["healthy"]]

            def pick(host, pool, exclude=None):
                for t in pool:
                    if t is exclude:
                        continue
                    if host is None or urllib.parse.urlsplit(t.base_url).hostname == host:
                        return t
                return None

            primary = pick(args.primary, [t for t in healthy if t.provider in ("openai", "openai_compat")] if args.primary is None else healthy)
            fallback = pick(args.fallback, healthy, exclude=primary)
            report = {"binary": args.binary, "primary": primary.describe() if primary else None, "fallback": fallback.describe() if fallback else None, "probes": probes, "results": []}
            if primary and fallback:
                suite = Suite(args.binary, primary, fallback, proxy, workroot, redact)
                oauth_key = None
                with contextlib.suppress(OSError, KeyError, ValueError):
                    oauth_key = read_secret(primary.key_ref)
                suite.run_all(oauth_key=oauth_key)
                sweep = [] if args.bad_key_hosts == "none" else [t for t in targets if t is not primary and (args.bad_key_hosts == "all" or urllib.parse.urlsplit(t.base_url).hostname in args.bad_key_hosts.split(","))]
                for target in sweep:
                    suite.bad_key(target, "bad_key")
                report["results"] = suite.results
            else:
                print("need two healthy targets; only %d healthy" % len(healthy), file=sys.stderr)
    finally:
        proxy.close()
        for fake in fakes:
            fake.close()
        os.umask(old_umask)
    os.makedirs(args.out, exist_ok=True)
    with open(os.path.join(args.out, "report.json"), "w") as handle:
        json.dump(report, handle, indent=2)
    with open(os.path.join(args.out, "report.md"), "w") as handle:
        handle.write(markdown(report))
    failed = [r for r in report["results"] if r["verdict"] == "FAIL"]
    print("\n%d scenarios, %d failed; report in %s" % (len(report["results"]), len(failed), args.out))
    return 1 if failed or not report["results"] else 0


if __name__ == "__main__":
    sys.exit(main())
