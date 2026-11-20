"""Compare the release gateway with its deterministic loopback upstream."""

import argparse
import concurrent.futures
import http.client
import json
import math
import os
from pathlib import Path
import queue
import select
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from contextlib import ExitStack, closing, contextmanager


ROOT = Path(__file__).resolve().parents[1]
TOKENS = 8
CLIENT_KEY = "mock-gateway"
UPSTREAM_KEY = "mock-backend"
TARGET_RPS = 1000
OVERHEAD_LIMIT_MS = 1.0
DEFAULT_FIXED_RATE_ROUNDS = 6


def build():
    output = subprocess.check_output(
        ["cargo", "build", "--locked", "--release", "-p", "llmux-server", "-p", "llmux-testkit",
         "--bin", "llmux-server", "--bin", "mock_load_upstream",
         "--message-format=json-render-diagnostics"],
        cwd=ROOT,
        text=True,
    )
    binaries = {}
    for line in output.splitlines():
        event = json.loads(line)
        if event["reason"] == "compiler-artifact" and event.get("executable"):
            binaries[event["target"]["name"]] = event["executable"]
    return binaries["mock_load_upstream"], binaries["llmux-server"]


@contextmanager
def server(binary, args, env):
    name = Path(binary).name
    with subprocess.Popen([binary, *args], env=env, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT) as process:
        try:
            if not select.select([process.stdout], [], [], 15)[0]:
                raise RuntimeError(f"{name} did not start within 15 seconds")
            line = process.stdout.readline().decode().strip()
            prefix = f"{name} listening on "
            if not line.startswith(prefix):
                raise RuntimeError(f"{name} failed to start: {line or 'no output'}")
            yield process.pid, line.removeprefix(prefix)
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()


@contextmanager
def target(binaries, via_gateway, hold, workers, delay_ms):
    mock, gateway = binaries
    env = dict(os.environ, TOKIO_WORKER_THREADS=str(workers),
               LLMUX_BENCH_CLIENT_KEY=CLIENT_KEY, LLMUX_BENCH_UPSTREAM_KEY=UPSTREAM_KEY)
    mock_args = ["--delay-ms", str(delay_ms)]
    if hold:
        mock_args.append("--hold")
    with server(mock, mock_args, env) as (upstream_pid, upstream):
        if not via_gateway:
            yield upstream, "backend-model", UPSTREAM_KEY, [upstream_pid]
            return
        with tempfile.TemporaryDirectory(prefix="llmux-bench-") as directory:
            config = Path(directory) / "llmux.toml"
            config.write_text(f"""
[server]
listen = "127.0.0.1:0"
client_keys = ["env:LLMUX_BENCH_CLIENT_KEY"]
request_timeout_ms = 120000

[routing]
max_attempts = 1

[[credentials]]
id = "mock"
provider = "openai_compat"
kind = "api_key"
key = "env:LLMUX_BENCH_UPSTREAM_KEY"

[[models]]
name = "gateway-model"
capabilities = {{ stream = true }}

[[models.deployments]]
id = "mock"
credential = "mock"
base_url = "http://{upstream}/v1"
upstream_model = "backend-model"
priority = 0
billing = "free"
""")
            with server(gateway, [str(config)], env) as (gateway_pid, address):
                yield address, "gateway-model", CLIENT_KEY, [upstream_pid, gateway_pid]


class Chat:
    def __init__(self, model, key, streaming):
        self.streaming = streaming
        self.body = json.dumps({
            "model": model,
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": streaming,
        }).encode()
        self.headers = {"Authorization": f"Bearer {key}", "Content-Type": "application/json"}

    def start(self, connection):
        connection.request("POST", "/v1/chat/completions", self.body, self.headers)
        response = connection.getresponse()
        expected = "text/event-stream" if self.streaming else "application/json"
        if response.status != 200 or response.headers.get_content_type() != expected:
            raise RuntimeError(f"expected 200 {expected}, got {response.status} "
                               f"{response.getheader('Content-Type')}")
        return response

    def request(self, connection, scheduled=None):
        start = time.perf_counter() if scheduled is None else scheduled
        first = None
        with self.start(connection) as response:
            if self.streaming:
                tokens = 0
                done = False
                for received, data in sse_data(response):
                    if done:
                        raise RuntimeError("unexpected event after [DONE]")
                    if data == b"[DONE]":
                        done = True
                    elif has_content(data):
                        tokens += 1
                        if first is None:
                            first = received - start
                if not done or tokens != TOKENS:
                    raise RuntimeError(f"incomplete stream: {tokens} tokens, [DONE]={done}")
            elif json.load(response)["choices"][0]["message"]["content"] != "x" * TOKENS:
                raise RuntimeError("unexpected mock completion")
        return time.perf_counter() - start, first

    def open_stream(self, connection):
        response = self.start(connection)
        for _, data in sse_data(response):
            if has_content(data):
                return response
        raise RuntimeError("held stream ended before its first token")


def sse_data(response):
    pending = b""
    while chunk := response.read1(16 * 1024):
        received = time.perf_counter()
        *events, pending = (pending + chunk).split(b"\n\n")
        for event in events:
            if event.startswith(b"data: "):
                yield received, event.removeprefix(b"data: ")
    if pending:
        raise RuntimeError("truncated SSE event")


def has_content(data):
    chunk = json.loads(data)
    if "error" in chunk:
        raise RuntimeError(f"mock stream error: {chunk['error']}")
    return any(choice.get("delta", {}).get("content") for choice in chunk["choices"])


def p99(samples):
    return sorted(samples)[math.ceil(len(samples) * 0.99) - 1]


def arrivals(requests, rate):
    start = time.perf_counter()
    for i in range(requests):
        scheduled = start + i / rate
        remaining = scheduled - time.perf_counter()
        if remaining > 0:
            time.sleep(remaining)
        yield scheduled


def fixed_rate(pool, chat, connections, requests, rate):
    available = queue.SimpleQueue()
    for connection in connections:
        available.put(connection)

    def request(scheduled):
        connection = available.get()
        try:
            latency, _ = chat.request(connection, scheduled=scheduled)
            return latency
        finally:
            available.put(connection)

    pending = [pool.submit(request, scheduled) for scheduled in arrivals(requests, rate)]
    return p99([future.result() for future in pending]) * 1000


def load(address, model, key, streaming, concurrency, requests, rate=None):
    chat = Chat(model, key, streaming)
    with ExitStack() as stack:
        with concurrent.futures.ThreadPoolExecutor(concurrency) as pool:
            connections = [
                stack.enter_context(closing(http.client.HTTPConnection(address, timeout=15)))
                for _ in range(concurrency)
            ]

            def warm(connection):
                for _ in range(4):
                    chat.request(connection)

            list(pool.map(warm, connections))
            if rate is not None:
                return fixed_rate(pool, chat, connections, requests, rate)
            start = None

            def start_timer():
                nonlocal start
                start = time.perf_counter()

            ready = threading.Barrier(concurrency, action=start_timer)
            per_worker, extra = divmod(requests, concurrency)

            def worker(item):
                i, connection = item
                ready.wait()
                samples = [chat.request(connection) for _ in range(per_worker + (i < extra))]
                return time.perf_counter(), samples

            results = list(pool.map(worker, enumerate(connections)))
    elapsed = max(end for end, _ in results) - start
    latency, first = zip(*(sample for _, samples in results for sample in samples))
    return p99(latency) * 1000, p99(first) * 1000 if streaming else None, requests / elapsed


def pss_kib(pids):
    total = 0
    for pid in pids:
        with open(f"/proc/{pid}/smaps_rollup") as stats:
            total += next(int(line.split()[1]) for line in stats if line.startswith("Pss:"))
    return total


def memory_per_stream(address, model, key, pids, concurrency):
    with closing(http.client.HTTPConnection(address, timeout=15)) as connection:
        Chat(model, key, False).request(connection)
    baseline = pss_kib(pids)
    chat = Chat(model, key, True)
    with ExitStack() as stack:
        with concurrent.futures.ThreadPoolExecutor(concurrency) as pool:
            connections = [
                stack.enter_context(closing(http.client.HTTPConnection(address, timeout=15)))
                for _ in range(concurrency)
            ]
            for response in pool.map(chat.open_stream, connections):
                stack.enter_context(closing(response))
        samples = []
        for _ in range(5):
            time.sleep(0.05)
            samples.append(pss_kib(pids))
    return (statistics.median(samples) - baseline) / concurrency


def round_order(index):
    return (False, True) if index % 2 == 0 else (True, False)


def paired_rounds(rounds, measure):
    pairs = []
    for index in range(rounds):
        results = {via_gateway: measure(via_gateway) for via_gateway in round_order(index)}
        pairs.append((results[False], results[True]))
    return pairs


def overhead_verdict(deltas):
    median = statistics.median(deltas)
    return median, "PASS" if median < OVERHEAD_LIMIT_MS else "FAIL"


def report_fixed_rate(pairs):
    print(f"\n### Fixed-rate JSON at {TARGET_RPS:,} RPS\n")
    print("| Round | Order | Direct p99 (ms) | Via gateway p99 (ms) | Gateway − direct (ms) |")
    print("| ---: | --- | ---: | ---: | ---: |")
    deltas = []
    for index, (direct, gateway) in enumerate(pairs):
        order = "Gateway → direct" if round_order(index)[0] else "Direct → gateway"
        delta = gateway - direct
        deltas.append(delta)
        print(f"| {index + 1} | {order} | {direct:.3f} | {gateway:.3f} | {delta:+.3f} |")
    median, result = overhead_verdict(deltas)
    print(f"\nJSON gateway p99 overhead target: **{result}** "
          f"(median paired delta {median:+.3f} ms; "
          f"range {min(deltas):+.3f} to {max(deltas):+.3f} ms; "
          f"target < {OVERHEAD_LIMIT_MS:g} ms, advisory).")


def compare(args):
    binaries = build()
    rows = []
    for via_gateway in (False, True):
        label = "via gateway" if via_gateway else "direct"
        print(f"Benchmarking {label} (JSON, SSE, held streams)…", file=sys.stderr)
        measurements = []
        for streaming in (False, True):
            with target(binaries, via_gateway, False, args.workers, args.upstream_delay_ms) as (
                address, model, key, _
            ):
                latency, first, throughput = load(address, model, key, streaming,
                                                  args.concurrency, args.requests)
                measurements.extend((latency, throughput))
                if streaming:
                    measurements.append(first)
        with target(binaries, via_gateway, True, args.workers, args.upstream_delay_ms) as (
            address, model, key, pids
        ):
            measurements.append(memory_per_stream(address, model, key, pids, args.concurrency))
        rows.append(measurements)

    def measure_fixed_rate(via_gateway):
        label = "via gateway" if via_gateway else "direct"
        print(f"Benchmarking {label} ({TARGET_RPS:,} RPS JSON)…", file=sys.stderr)
        with target(binaries, via_gateway, False, args.workers, args.upstream_delay_ms) as (
            address, model, key, _
        ):
            return load(address, model, key, False, args.concurrency, args.requests, rate=TARGET_RPS)

    pairs = paired_rounds(args.fixed_rate_rounds, measure_fixed_rate)
    print(f"{args.requests} requests per route and mode, {args.concurrency} workers/held "
          f"streams, {args.workers} Tokio workers per server process, "
          f"{args.upstream_delay_ms} ms configured upstream delay, "
          f"{args.fixed_rate_rounds} fixed-rate rounds.\n")
    print("| Metric | Direct | Via gateway | Gateway − direct |")
    print("| --- | ---: | ---: | ---: |")
    labels = (
        "JSON p99 latency (ms)", "JSON throughput (req/s)",
        "SSE p99 completion latency (ms)", "SSE throughput (streams/s)",
        "SSE p99 time to first token (ms)", "Incremental PSS per held stream (KiB)",
    )
    for label, direct, gateway in zip(labels, *rows):
        print(f"| {label} | {direct:.3f} | {gateway:.3f} | {gateway - direct:+.3f} |")
    report_fixed_rate(pairs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--requests", type=int, default=4096, help="timed requests per route and mode")
    parser.add_argument("--concurrency", type=int, default=64, help="load workers and held streams")
    parser.add_argument("--workers", type=int, default=2, help="Tokio workers per server process")
    parser.add_argument("--fixed-rate-rounds", type=int, default=DEFAULT_FIXED_RATE_ROUNDS,
                        help="paired 1,000 RPS JSON rounds (minimum 2, default: %(default)s)")
    parser.add_argument("--upstream-delay-ms", type=int, default=0,
                        help="inject a fixed delay before each mock response")
    args = parser.parse_args()
    if not sys.platform.startswith("linux") or not Path("/proc/self/smaps_rollup").is_file():
        parser.error("Linux with /proc/<pid>/smaps_rollup is required for PSS measurement")
    if not 1 <= args.concurrency <= args.requests or args.workers < 1:
        parser.error("require 1 <= concurrency <= requests and workers >= 1")
    if args.fixed_rate_rounds < 2:
        parser.error("require at least 2 fixed-rate rounds")
    if not 0 <= args.upstream_delay_ms < 15000:
        parser.error("upstream delay must be nonnegative and below the 15-second client timeout")
    compare(args)


if __name__ == "__main__":
    main()
