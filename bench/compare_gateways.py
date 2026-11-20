#!/usr/bin/env python3
"""Bounded, sequential gateway comparison. Python 3.9+, standard library only."""

import argparse
from collections import Counter
from datetime import datetime, timezone
import html
import http.client
import json
import math
import os
from pathlib import Path
import re
import socket
import statistics
import sys
import time
from urllib.parse import urlsplit


MAX_BODY = 1024 * 1024
MAX_EVENT = 64 * 1024
PROMPT = "Reply with just OK."


def generate_cases(models, responses=0):
    if not models or any(not isinstance(m, str) or not m.strip() for m in models):
        raise ValueError("provide at least one nonempty model name")
    models = list(dict.fromkeys(models))
    if not 0 <= responses <= len(models):
        raise ValueError("responses count must be between zero and the model count")
    cases = []
    for model in models:
        for kind in ("chat", "stream"):
            body = dict(model=model, messages=[dict(role="user", content=PROMPT)],
                        max_tokens=16, stream=kind == "stream")
            cases.append(dict(model=model, kind=kind, path="/chat/completions",
                              body=json.dumps(body, separators=(",", ":")).encode()))
    for model in models[:responses]:
        body = dict(model=model, input=PROMPT, max_output_tokens=16)
        cases.append(dict(model=model, kind="responses", path="/responses",
                          body=json.dumps(body, separators=(",", ":")).encode()))
    return cases


def make_plan(cases, gateways, cap):
    if len(gateways) != 2 or len(set(gateways)) != 2:
        raise ValueError("provide two distinct gateway names")
    if cap < 1 or len(cases) * 2 > cap:
        raise ValueError("plan needs %d HTTP requests; cap is %d (nothing sent)" % (len(cases) * 2, cap))
    return [(case, name) for i, case in enumerate(cases)
            for name in (gateways if i % 2 == 0 else gateways[::-1])]


def coverage_models(data, top):
    rows = data.get("rows") if isinstance(data, dict) else None
    if not isinstance(rows, list) or top < 1:
        raise ValueError("coverage/traffic JSON needs a rows array and positive --top")
    if any(not isinstance(r, dict) or not isinstance(r.get("name"), str)
           or not isinstance(r.get("calls"), (float, int)) for r in rows):
        raise ValueError("coverage rows need name and calls")
    ranked = sorted(rows, key=lambda r: r["calls"], reverse=True)[:top]
    return [r["name"] for r in ranked if r.get("present", True)]


def read_key(ref):
    if ref.startswith("file:"):
        value = Path(ref[5:]).read_text().strip()
    elif ref.startswith("env:"):
        value = os.environ.get(ref[4:], "").strip()
    else:
        raise ValueError("gateway key must use file:PATH or env:VARIABLE, never a literal")
    if not value or any(ord(c) < 33 or ord(c) > 126 for c in value):
        raise ValueError("missing or invalid gateway key")
    return value


def validate_url(url):
    parsed = urlsplit(url)
    if (parsed.scheme not in ("http", "https") or not parsed.hostname or parsed.username
            or parsed.password or parsed.query or parsed.fragment):
        raise ValueError("base URL must be HTTP(S), without credentials, query or fragment")
    try:
        parsed.port
    except ValueError:
        raise ValueError("invalid base URL port") from None
    return parsed


def redact(text, secrets=()):
    text = str(text)
    for secret in secrets:
        if secret:
            text = text.replace(secret, "[REDACTED]")
    text = re.sub(r"https?://[^\s<>\"']+", "[URL]", text, flags=re.I)
    text = re.sub(r"(?i)\b(?:bearer|basic)\s+[^\s,;\"']+", "[REDACTED]", text)
    text = re.sub(r"(?i)\b(?:sk|pk|api[-_]?key|token|secret)[-_:][a-z0-9_./+=-]+",
                  "[REDACTED]", text)
    text = re.sub(r"(?i)\b(?:key|token|secret|authorization)\s*[=:]\s*[^\s,;\"']+",
                  "[REDACTED]", text)
    text = re.sub(r"\b[A-Za-z0-9_+/=]{24,}\b", "[REDACTED]", text)
    return " ".join(text.split())[:400]


def classify(status, code=None, transport=None):
    if transport:
        return transport
    if status == 429 or code in ("429", "rate_limit_exceeded", "rate_limited", "insufficient_quota"):
        return "rate_limited"
    if status in (401, 403) or code in ("unknown_model", "context_length_exceeded",
                                      "invalid_api_key", "unauthorized", "unsupported_format"):
        return "gateway_reject"
    if (status is not None and status >= 500) or code in ("500", "502", "503", "504", "server_error"):
        return "upstream_5xx"
    if code is not None or (status is not None and status >= 400):
        return "upstream_4xx"
    return "ok" if status is not None and 200 <= status < 300 else "gateway_reject"


def error_fields(data):
    if not isinstance(data, dict) or data.get("error") is None:
        return None, None
    error = data["error"]
    if isinstance(error, dict):
        return str(error.get("code") or error.get("type") or "upstream_error"), str(error.get("message", "error"))
    return "upstream_error", str(error)


def routing_headers(headers, secrets):
    allowed = {
        'x-llmux-deployment', 'x-llmux-attempts', 'x-llmux-dropped-messages',
        'x-request-id', 'x-provider', 'x-model', 'x-combo', 'x-fallback', 'x-fallback-count',
        'x-upstream-provider', 'x-model-alias-resolved',
        'x-omniroute-route-class', 'x-omniroute-route-decision', 'x-omniroute-routed-by',
        'x-omniroute-fallback-attempts', 'x-omniroute-fallback-used',
        'x-omniroute-emergency-fallback', 'x-omniroute-reasoning-fallback',
        'x-omniroute-budget-fallback', 'x-omniroute-provider', 'x-omniroute-model',
        'x-omniroute-connection', 'x-omniroute-selected-connection-id',
        'x-omniroute-combo-attempted', 'x-omniroute-combo-excluded',
        'x-omniroute-combo-pool-size', 'x-omniroute-combo-terminal-reason',
        'x-omniroute-combo-trace', 'x-omniroute-local-cooldown',
        'x-omniroute-retry-after-seconds', 'x-omniroute-decision',
    }
    return {k.lower(): redact(v, secrets) for k, v in headers
            if k.lower() in allowed}


def send(gateway, case, timeout, secrets):
    start = time.monotonic()
    deadline = start + timeout
    parsed = validate_url(gateway["base_url"])
    connection_type = http.client.HTTPSConnection if parsed.scheme == "https" else http.client.HTTPConnection
    conn = connection_type(parsed.hostname, parsed.port, timeout=timeout)
    record = dict(gateway=gateway["name"], model=case["model"], kind=case["kind"],
                  status=None, error_class=None, error=None, latency_ms=None, ttft_ms=None,
                  done=False if case["kind"] == "stream" else None, deployment=None,
                  attempts=None, fallback=None, routing_headers={}, started_at=datetime.now(timezone.utc).isoformat())
    code = message = transport = None
    response = None

    def remaining():
        left = deadline - time.monotonic()
        if left <= 0:
            raise socket.timeout()
        # HTTP/1.0 responses detach the socket from HTTPConnection after headers.
        if conn.sock:
            conn.sock.settimeout(left)
        elif response and response.fp:
            response.fp.raw._sock.settimeout(left)
        return left

    try:
        conn.request("POST", parsed.path.rstrip("/") + case["path"], body=case["body"],
                     headers={"Authorization": "Bearer " + gateway["key"],
                              "Content-Type": "application/json", "Accept-Encoding": "identity"})
        remaining()
        response = conn.getresponse()
        record["status"] = response.status
        record["routing_headers"] = routing_headers(response.getheaders(), secrets)
        record["deployment"] = record["routing_headers"].get("x-llmux-deployment")
        attempts = record["routing_headers"].get("x-llmux-attempts", "")
        if attempts.isdigit():
            record["attempts"] = int(attempts)
            record["fallback"] = int(attempts) > 1
        raw = bytearray()
        pending = bytearray()
        event = []
        is_sse = case["kind"] == "stream" and 200 <= response.status < 300
        def consume_event():
            nonlocal code, message
            if not event:
                return
            data = b"\n".join(event).decode("utf-8", errors="replace")
            event.clear()
            if data.strip() == "[DONE]":
                record["done"] = True
                return
            obj = json.loads(data)
            event_code, event_message = error_fields(obj)
            if event_code:
                code, message = event_code, event_message
            if not isinstance(obj, dict) or not isinstance(obj.get("choices", []), list):
                raise ValueError("invalid SSE payload")
            for choice in obj.get("choices", []):
                if not isinstance(choice, dict):
                    raise ValueError("invalid SSE choice")
                delta = choice.get("delta", {})
                if isinstance(delta, dict) and delta.get("content") and record["ttft_ms"] is None:
                    record["ttft_ms"] = round((time.monotonic() - start) * 1000, 3)

        while True:
            remaining()
            chunk = response.read1(4096)
            if not chunk:
                break
            if is_sse:
                pending.extend(chunk)
                while b"\n" in pending:
                    line, _, tail = pending.partition(b"\n")
                    pending = bytearray(tail)
                    line = line.rstrip(b"\r")
                    if not line:
                        consume_event()
                    elif line.startswith(b"data:"):
                        event.append(line[5:].lstrip(b" "))
                    if sum(map(len, event)) > MAX_EVENT:
                        raise ValueError("SSE event exceeds limit")
                if len(pending) > MAX_EVENT:
                    raise ValueError("SSE line exceeds limit")
                if record["done"]:
                    break
            else:
                raw.extend(chunk)
                if len(raw) > MAX_BODY:
                    raise ValueError("response exceeds limit")
        if is_sse:
            if not record["done"] and not code:
                transport, message = "connection", "stream ended without [DONE]"
        else:
            try:
                data = json.loads(raw)
            except ValueError:
                data = None
            code, message = error_fields(data)
            if not 200 <= response.status < 300 and message is None:
                message = "HTTP %d (non-JSON error body omitted)" % response.status
            elif code is None and 200 <= response.status < 300:
                valid = isinstance(data, dict) and (
                    isinstance(data.get("choices"), list) and bool(data["choices"])
                    if case["kind"] != "responses" else
                    isinstance(data.get("output"), list) and bool(data["output"]))
                if not valid:
                    transport, message = "connection", "invalid success payload (body omitted)"
    except (socket.timeout, TimeoutError):
        transport, message = "timeout", "request deadline exceeded"
    except (OSError, http.client.HTTPException):
        transport, message = "connection", "connection or HTTP framing failure"
    except ValueError:
        transport, message = "connection", "invalid or oversized response (body omitted)"
    finally:
        if response:
            response.close()
        conn.close()
    record["latency_ms"] = round((time.monotonic() - start) * 1000, 3)
    record["error_class"] = classify(record["status"], code, transport)
    if record["error_class"] != "ok":
        record["error"] = redact(message or "HTTP %s" % record["status"], secrets)
    return record


def verdict(a_ok, b_ok):
    if a_ok and b_ok:
        return "match"
    if a_ok:
        return "llmux-only fail"
    if b_ok:
        return "omniroute-only fail"
    return "both fail"


def aggregate(results, gateways):
    pairs = {}
    for row in results:
        key = (row["model"], row["kind"])
        pair = pairs.setdefault(key, {})
        if row["gateway"] not in gateways or row["gateway"] in pair:
            raise ValueError("unknown gateway or duplicate result")
        pair[row["gateway"]] = row
    paired = []
    models = {}
    for (model, kind), pair in pairs.items():
        if set(pair) != set(gateways):
            raise ValueError("unpaired results")
        a, b = (pair[g] for g in gateways)
        item = dict(model=model, kind=kind, results=pair,
                    verdict=verdict(a["error_class"] == "ok", b["error_class"] == "ok"),
                    same_error_class=a["error_class"] == b["error_class"])
        paired.append(item)
        models.setdefault(model, []).append(item)
    summaries = {}
    for gateway in gateways:
        rows = [r for r in results if r["gateway"] == gateway]
        successful = [r for r in rows if r["error_class"] == "ok"]

        def median(values):
            return round(statistics.median(values), 3) if values else None

        summaries[gateway] = dict(requests=len(rows), successes=len(successful),
                                  errors=dict(Counter(r["error_class"] for r in rows if r["error_class"] != "ok")),
                                  p50_ms=median([r["latency_ms"] for r in rows]),
                                  success_p50_ms=median([r["latency_ms"] for r in successful]),
                                  success_ttft_p50_ms=median([r["ttft_ms"] for r in successful if r["ttft_ms"] is not None]))
    model_rows = []
    for model, cases in models.items():
        ok = [all(p["results"][g]["error_class"] == "ok" for p in cases) for g in gateways]
        model_rows.append(dict(model=model, verdict=verdict(*ok), cases=cases))
    return dict(gateways=gateways, summary=dict(requests=len(results), pairs=len(paired),
                verdicts=dict(Counter(p["verdict"] for p in paired)), gateways=summaries),
                models=model_rows, pairs=paired)


def render_html(report):
    escape = lambda s: html.escape(str(s))
    gateways = report["gateways"]

    def display(value):
        if value is None:
            return '—'
        if isinstance(value, float):
            value = format(value, '.3f').rstrip('0').rstrip('.')
        return escape(value)

    def badge(verdict, count=None):
        label = verdict if count is None else verdict+': '+str(count)
        return '<span class="badge '+('ok' if verdict == 'match' else 'fail')+'">'+escape(label)+'</span>'

    pieces = ['<!doctype html><html lang="en"><meta charset="utf-8">',
              '<meta name="viewport" content="width=device-width,initial-scale=1">',
              '<title>Gateway comparison</title><style>',
              'body{font:15px system-ui,sans-serif;margin:2rem;color:#17212b;background:#f6f8fa}',
              'table{border-collapse:collapse;width:100%;background:white;margin:1rem 0}',
              'th,td{border:1px solid #c9d1d9;padding:.65rem;text-align:left;vertical-align:top}',
              'small{display:block;overflow-wrap:anywhere}pre{white-space:pre-wrap}',
              '.ok{color:#176734}.fail{color:#a32620}.badge{display:inline-block;padding:.15rem .45rem;border-radius:.3rem}',
              '.badge.ok{background:#e0f2e5}.badge.fail{background:#fde7e5}.strip{display:flex;gap:1rem;flex-wrap:wrap}',
              'dl{display:grid;grid-template-columns:max-content 1fr;gap:.35rem 1rem}dd{margin:0;overflow-wrap:anywhere}',
              'h1{margin-bottom:.4rem}caption{text-align:left;color:#52606d;margin:.3rem 0}</style>',
              '<h1>Gateway comparison</h1><p>Single sequential snapshot, not a performance ranking. ',
              'Both gateways receive identical bytes; pair order alternates. No automatic retries. ',
              'Fast errors are not successful work. TTFT starts at request dispatch and ends at the first content delta.</p>',
              '<p>Verdicts use success across each pair; both failures are never a match, even with equal error classes. ',
              'Model verdict requires every case to succeed; failures on both sides across different cases count as both fail. ',
              '“llmux-only” means the second gateway; “omniroute-only” means the first.</p>',
              '<h2>Summary</h2><div class="strip" aria-label="Verdict counts">']
    for verdict in ('match', 'llmux-only fail', 'omniroute-only fail', 'both fail'):
        pieces.append(badge(verdict, report['summary']['verdicts'].get(verdict, 0)))
    pieces.append('</div><table aria-label="Summary"><thead><tr><th>Metric</th>'+''.join(
        '<th>'+escape(g)+'</th>' for g in gateways)+'</tr></thead><tbody>')
    summaries = [report['summary']['gateways'][g] for g in gateways]
    metrics = [('Successes / requests', [str(s['successes'])+' / '+str(s['requests']) for s in summaries]),
               ('Error classes', [', '.join(k+': '+str(v) for k, v in sorted(s['errors'].items())) or '—' for s in summaries])]
    metrics.extend((label, [s[key] for s in summaries]) for label, key in (
        ('p50 all (ms)', 'p50_ms'), ('p50 success (ms)', 'success_p50_ms'),
        ('p50 TTFT (ms)', 'success_ttft_p50_ms')))
    for label, values in metrics:
        pieces.append('<tr><th>'+label+'</th>'+''.join('<td>'+display(v)+'</td>' for v in values)+'</tr>')
    pieces.append('</tbody></table><h2>Models</h2><table aria-label="Models"><caption>HTTP '+
                  escape(' / '.join(gateways))+'; colors indicate success, including payload and stream validation.</caption>'+
                  '<thead><tr><th>Model</th><th>Chat</th><th>Stream</th><th>Responses</th><th>Verdict</th></tr></thead><tbody>')
    for model in report['models']:
        pieces.append('<tr><th>'+escape(model['model'])+'</th>')
        cases = {c['kind']: c for c in model['cases']}
        for kind in ('chat', 'stream', 'responses'):
            case = cases.get(kind)
            if case is None:
                pieces.append('<td>—</td>')
            else:
                pieces.append('<td class="'+('ok' if case['verdict'] == 'match' else 'fail')+'">'+
                              ' / '.join(display(case['results'][g]['status']) for g in gateways)+'</td>')
        pieces.append('<td>'+badge(model['verdict'])+'</td></tr>')
    pieces.append('</tbody></table><h2>Run details</h2><dl>')
    for key, label in (('completed_at', 'Completed (UTC)'), ('timeout_s', 'Timeout (seconds)'),
                       ('total_http_requests_sent', 'HTTP requests sent'), ('compared_requests', 'Compared requests'),
                       ('superseded_requests', 'Superseded requests'), ('max_requests', 'Request cap'),
                       ('ordering', 'Order'), ('note', 'Note')):
        if report.get('run', {}).get(key) is not None:
            pieces.append('<dt>'+label+'</dt><dd>'+display(report['run'][key])+'</dd>')
    pieces.extend(['</dl><p>p50 TTFT uses successful streams only. Fallback unknown ≠ false; routing headers are evidence, ',
                   'not a reconstructed attempt trace. Error classes are heuristics, not proof of error provenance.</p>',
                   '<h2>Request details</h2><table aria-label="Request details"><thead><tr><th>Model / case</th><th>'+escape(gateways[0])+ '</th><th>'+escape(gateways[1])+'</th><th>Verdict</th></tr></thead><tbody>'])
    for model in report["models"]:
        pieces.append('<tr><th colspan="3">'+escape(model["model"])+'</th><th>'+escape(model["verdict"])+'</th></tr>')
        for case in model["cases"]:
            pieces.append('<tr><td>'+escape(case["kind"])+"</td>")
            for gateway in gateways:
                r = case["results"][gateway]
                fields = ['HTTP '+display(r['status']), display(r['error_class']), display(r['latency_ms'])+' ms']
                if r['ttft_ms'] is not None:
                    fields.append('TTFT '+display(r['ttft_ms'])+' ms')
                if case['kind'] == 'stream':
                    fields.append('DONE '+('✓' if r['done'] else '✗'))
                if gateway == gateways[1] and r['attempts'] is not None:
                    attempts = r['attempts']
                    fields.append(str(attempts)+(' attempt' if attempts == 1 else ' attempts')+
                                  (' (fallback)' if attempts > 1 else ''))
                pieces.append('<td class="'+('ok' if r['error_class'] == 'ok' else 'fail')+'">'+' · '.join(fields))
                if r['deployment']:
                    pieces.append('<small>Deployment '+escape(r['deployment'])+'</small>')
                if r['error']:
                    pieces.append('<small>'+escape(r['error'])+'</small>')
                if r['routing_headers']:
                    pieces.append('<details><summary>Routing headers</summary><pre>'+
                                  escape(json.dumps(r['routing_headers'], indent=2))+'</pre></details>')
                pieces.append('</td>')
            pieces.append('<td>'+badge(case['verdict'])+'<small>Same error class: '+
                          ('yes' if case['same_error_class'] else 'no')+'</small></td></tr>')
    if not report["models"]:
        pieces.append('<tr><td colspan="4">No results</td></tr>')
    pieces.append('</tbody></table></html>')
    return ''.join(pieces)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--gateway', action='append', nargs=3, metavar=('NAME', 'BASE_URL', 'KEY_REF'), required=True,
                        help='exactly twice, reference first then candidate; BASE_URL includes /v1')
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument('--model', action='append', help='repeat for each model')
    source.add_argument('--models-file', type=Path, help='JSON string array or one model per line')
    source.add_argument('--coverage', type=Path, help='coverage/traffic JSON; use imported names among top traffic rows')
    parser.add_argument('--top', type=int, default=25)
    parser.add_argument('--responses', type=int, default=0, help='Responses cases for the first N selected models')
    parser.add_argument('--timeout', type=float, default=45, help='total per-request seconds, including stream (DNS may exceed this)')
    parser.add_argument('--max-requests', type=int, default=80, help='HTTP request cap, not upstream-attempt cap; over-budget plans fail before sending')
    parser.add_argument('--output', type=Path, required=True, help='new output directory; never overwrites a run')
    args = parser.parse_args(argv)
    try:
        if not math.isfinite(args.timeout) or args.timeout <= 0:
            raise ValueError('timeout must be positive and finite')
        if args.coverage:
            models = coverage_models(json.loads(args.coverage.read_text()), args.top)
        elif args.models_file:
            text = args.models_file.read_text()
            models = json.loads(text) if text.lstrip().startswith('[') else [l.strip() for l in text.splitlines() if l.strip()]
            if not isinstance(models, list):
                raise ValueError('models JSON must be an array')
        else:
            models = args.model
        cases = generate_cases(models, args.responses)
        gateways = {}
        for name, url, ref in args.gateway:
            validate_url(url)
            if name in gateways:
                raise ValueError('gateway names must be unique')
            gateways[name] = dict(name=name, base_url=url, key=read_key(ref))
        plan = make_plan(cases, list(gateways), args.max_requests)
        args.output.mkdir(mode=0o700)
    except (ValueError, OSError):
        # Do not echo argparse values, paths, URLs or invalid input that might contain secrets.
        print('Invalid inputs, over-budget plan, unreadable key/model file, or output already exists. Nothing sent.', file=sys.stderr)
        return 2
    secrets = [g['key'] for g in gateways.values()]
    results = []
    print('Sending %d HTTP requests (%d paired cases), cap %d.' % (len(plan), len(cases), args.max_requests), flush=True)
    for i, (case, name) in enumerate(plan):
        results.append(send(gateways[name], case, args.timeout, secrets))
        # A private checkpoint contains results only, never request bodies or credentials.
        checkpoint = args.output / 'partial.json'
        with checkpoint.open('w', encoding='utf-8') as f:
            json.dump(results, f, indent=2)
        checkpoint.chmod(0o600)
        print('%d/%d completed' % (i+1, len(plan)), flush=True)
    report = aggregate(results, list(gateways))
    report['run'] = dict(completed_at=datetime.now(timezone.utc).isoformat(), timeout_s=args.timeout,
                         max_requests=args.max_requests, ordering='alternating pairs; sequential; no retries',
                         latency_note='one sample per gateway/case; includes errors; no speedup inference')
    for filename, content in [('results.json', json.dumps(report, indent=2)), ('report.html', render_html(report))]:
        for secret in secrets:
            content = content.replace(secret, '[REDACTED]')
        path = args.output / filename
        with path.open('x', encoding='utf-8') as f:
            f.write(content)
        path.chmod(0o600)
    checkpoint.unlink()
    print(json.dumps(report['summary'], indent=2))
    return 0


if __name__ == '__main__':
    sys.exit(main())
