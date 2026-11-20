# llmux2

Rust gateway exposing multiple API-key upstreams under one OpenAI-compatible model name. The MVP supports JSON and SSE at `POST /v1/chat/completions`.

## Run

1. Copy `examples/llmux.example.toml` to `llmux.toml` and set the upstream base URLs and model IDs. Base URLs include the provider's API prefix, such as `/v1`; the adapter appends `/chat/completions`.
2. Set the secrets named by the config in your environment (or use `file:/path/to/secret` references). The gateway rejects inline keys. Use a separate key for gateway clients.
3. Start the server with `cargo run --locked -- llmux.toml`. It listens on loopback by default; if exposing it remotely, use a TLS reverse proxy.

```sh
curl -sS http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $LLMUX_CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"llama-70b","messages":[{"role":"user","content":"Hello"}]}'
```

For SSE, add `"stream":true` to the request and use `curl -N`. The gateway rewrites just the top-level request model to the upstream ID; other request fields and successful response payloads pass through.

## Routing

Lower numeric priority wins, with round-robin ordering among equal priorities. On an upstream HTTP 429, the gateway tries the next deployment and skips the limited deployment until its `Retry-After` (seconds or HTTP date) expires. Missing values use the configured default, and waits are capped by `cooldown.max_ms`. If none can serve the request, the gateway returns 429 with the earliest known retry delay. Optional local per-deployment RPM and concurrency limits also skip deployments without contacting them.

`x-llmux-deployment` and `x-llmux-attempts` identify the winning deployment and number of HTTP attempts (not skipped deployments). Set `server.expose_routing_headers = false` to hide them. A stream commits on its first complete SSE event; after that, failures end the stream with an error payload rather than replaying the request.

The MVP does not wire OAuth into server configuration, or implement multi-format routing, hot reload, circuit breakers, or failover on errors other than 429. The providers crate offers an OAuth 2.0 refresh-token credential source for programmatic use. The server uses bounded JSON bodies and SSE events; request timeouts cover upstream setup/first event, then apply as an idle timeout between events. Local RPM uses a bounded per-deployment sliding window only when configured.

## Development

- `llmux-types` defines the shared model, IR, and provider contracts.
- `llmux-config` parses and validates TOML and secret references.
- `llmux-core` selects deployments, enforces limits, and executes attempts.
- `llmux-providers` implements the OpenAI-compatible and Anthropic Messages adapters with static API-key and OAuth refresh-token credential sources (OAuth configuration wiring is not yet available), including Responses-to-Chat-Completions conversion (JSON and SSE); server routing for `/v1/responses` is not yet wired. Stateful Responses conversations and built-in non-function tools are unsupported.
- `llmux-server` assembles those crates and serves the HTTP API.
- `llmux-testkit` provides loopback HTTP fixtures.

```sh
cargo test --locked --workspace
cargo test --locked -p llmux-server --test chat -- --nocapture
```

## Mock-upstream load benchmark

On Linux with Python 3.10+, Cargo, and readable `/proc/<pid>/smaps_rollup`, run:

```sh
python3 bench/mock_upstream.py
```

The command builds the release gateway and the `llmux-testkit` Axum mock, starts and stops them
on ephemeral loopback ports with dummy API keys, and prints Markdown tables
with direct, via-gateway, and via-minus-direct results. Use `--requests`,
`--concurrency`, and `--workers` to tune the run, or `--upstream-delay-ms` to
inject a fixed delay before every mock response. Defaults: no artificial delay, 4,096 timed
requests per route and mode, 64 concurrent clients/held streams, and two Tokio
workers per server process.

- **Latency and throughput:** Each client warms its HTTP/1.1 connection with
  four requests, then runs a share of the timed requests. p99 is the nearest-rank
  end-to-end completion time, reported separately for JSON and SSE. Throughput
  divides completed requests by elapsed wall time. SSE p99 time to first token
  measures from request start to the first complete content-bearing event, not
  the role-only event. The mock sends eight content chunks and `[DONE]`.
  These are short-response, single-host measurements and the load generator
  can limit throughput.
- **1,000 RPS JSON target:** An independent arrival schedule offers one request
  per millisecond, without waiting for previous responses. Latency is measured
  from each scheduled arrival through completion, so client queueing is
  included when workers lag. One untimed direct/gateway warm-up pair is
  discarded before nine timed pairs alternate the run order. With default
  settings the fixed-rate workload takes about 82 seconds to schedule. Use
  `--fixed-rate-rounds` to change the number of timed pairs (at least two;
  odd counts select an observed pair as the median). The report shows each
  round's gateway-minus-direct p99, the range of those differences, and
  separate direct and gateway p99 medians. The median of the paired
  differences, not the difference of the route medians, is compared to the
  design's **< 1 ms** target. The verdict is advisory, not a CI gate; a wide
  range signals noisy measurements.
- **Memory:** In fresh processes primed with one JSON request, capture idle
  proportional set size (PSS), open the configured number of streams, and hold
  them after their first content event. Report the median of five PSS samples
  minus idle PSS, divided by the number of held streams. Direct measures the
  mock process; via-gateway sums the mock and gateway PSS. The difference is
  added userspace memory per stream at that concurrency, including connection
  and allocator overhead, but excluding the client and kernel socket buffers.
