# llmux2

Rust gateway exposing multiple API-key upstreams under one OpenAI-compatible model name. Both `POST /v1/chat/completions` and `POST /v1/responses` support JSON and SSE.

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

For Chat Completions on OpenAI-compatible upstreams, the gateway rewrites only the top-level request model. Other request fields and successful response payloads pass through.

```sh
curl -sS http://127.0.0.1:8080/v1/responses \
  -H "Authorization: Bearer $LLMUX_CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"llama-70b","input":"Hello"}'
```

The Responses route translates requests to Chat Completions on OpenAI-compatible upstreams and converts replies back. Stateful Responses conversations and built-in non-function tools are unsupported. For SSE on either route, add `"stream":true` and use `curl -N`.

Credentials use `provider = "openai"` or `"openai_compat"` for OpenAI-compatible upstreams, or `provider = "anthropic"` for the Anthropic Messages API. Anthropic deployments use a base URL such as `https://api.anthropic.com/v1` (the adapter appends `/messages`) and send the key as `x-api-key`. The gateway converts chat requests, JSON responses and SSE chunks between the OpenAI Chat and Anthropic formats, so a model can mix both kinds of deployments and fail over between them.

## Routing

Lower numeric priority wins, with round-robin ordering among equal priorities. On an upstream HTTP 429, the gateway tries the next deployment and skips the limited deployment until its `Retry-After` (seconds or HTTP date) expires. Missing values use the configured default, and waits are capped by `cooldown.max_ms`. If none can serve the request, the gateway returns 429 with the earliest known retry delay. Optional local per-deployment RPM and concurrency limits also skip deployments without contacting them.

Each deployment may set `max_context` (prompt plus completion tokens), defaulting to the model's `capabilities.max_context`. Before routing, the gateway estimates prompt tokens (about 4 bytes of prompt text per token, ignoring base64 `data:` URLs) plus the requested output limit (`max_output_tokens` for Responses, `max_completion_tokens` else `max_tokens` for Chat), and skips deployments of that model that it would not fit. It never falls back to a different model: if no deployment fits, it returns 400 `context_length_exceeded`, naming the limit, without contacting upstream.

Rejecting is the default. To trim instead, set `context_overflow = "middle-out"` on a model, or send `x-llmux-context-overflow: middle-out` (or `reject`) per request. The gateway then drops messages (Responses `input` items) from the middle of the conversation until the request fits the largest window. It keeps system and developer messages and the latest message, and drops tool results together with the call before them. The response carries `x-llmux-dropped-messages: <n>`, so a trim is never silent. If dropping every middle message still doesn't fit, the request gets the same 400.

`x-llmux-deployment` and `x-llmux-attempts` identify the winning deployment and number of HTTP attempts (not skipped deployments). Set `server.expose_routing_headers = false` to hide them. A stream commits on its first complete SSE event; after that, failures end the stream with an error payload rather than replaying the request.

The MVP does not implement multi-format routing, hot reload, circuit breakers, or failover on errors other than 429. The server uses bounded JSON bodies and SSE events; request timeouts cover upstream setup/first event, then apply as an idle timeout between events. Local RPM uses a bounded per-deployment sliding window only when configured.

## Credentials

Credentials support `kind = "api_key"` (with a `key` secret reference) and
`kind = "oauth"` (OAuth 2.0 refresh-token grant). Deployments select either by
credential `id`. Both use `provider = "openai"` or `"openai_compat"` in the server.

```toml
[[credentials]]
id = "oauth-account"
kind = "oauth"
provider = "openai_compat"
token_url = "https://auth.example.com/oauth/token"
client_id = "gateway-client"
refresh_token = "env:UPSTREAM_REFRESH_TOKEN"
client_secret = "env:UPSTREAM_CLIENT_SECRET" # optional for public clients
scopes = ["inference"] # optional
refresh_skew_ms = 60000 # optional; default 60 seconds, zero allowed
request_timeout_ms = 30000 # optional; default 30 seconds
```

`token_url`, `client_id`, and `refresh_token` are required and nonempty. Secrets
must be `env:` or `file:` references, never inline values. Token endpoints require
HTTPS (HTTP is allowed only on loopback), with no userinfo, query or fragment.
Timeouts are 1–86400000 ms; refresh skew is 0–86400000 ms. OAuth startup fetches
the initial access token and fails if that request fails. Access tokens are cached
and refreshed in the background; rotated refresh tokens are kept in memory only,
so persist them externally before restarting if your provider invalidates old ones.
Authentication-error fallback / on-demand refresh retries are not implemented.

## Development

- `llmux-types` defines the shared model, IR, and provider contracts.
- `llmux-config` parses and validates TOML and secret references.
- `llmux-core` selects deployments, enforces limits, and executes attempts.
- `llmux-providers` implements the OpenAI-compatible and Anthropic Messages adapters with static API-key and OAuth refresh-token credential sources, including Responses-to-Chat-Completions conversion.
- `llmux-server` assembles those crates and serves the HTTP API.
- `llmux-testkit` provides loopback HTTP fixtures.

```sh
cargo test --locked --workspace
cargo test --locked -p llmux-server --test chat -- --nocapture
```

## Live gateway comparison

`bench/compare_gateways.py` runs on Python 3.9+ using only the standard library.
Run on the gateway host so credentials stay there. Supply the **reference first**
(OmniRoute), then the candidate (llmux). URLs include the OpenAI API prefix.
Keys must be `file:/absolute/path` or `env:VARIABLE`, never literal command-line
arguments. Use existing authorized client keys; this tool never provisions keys.

```sh
python3 bench/compare_gateways.py \
  --gateway omniroute http://127.0.0.1:20128/v1 file:/private/omniroute-client.key \
  --gateway llmux http://127.0.0.1:8090/v1 file:/private/llmux-client.key \
  --coverage /private/coverage.json --top 25 --responses 2 \
  --timeout 45 --max-requests 80 --output /private/compare-run-001
```

Alternatively use repeated `--model NAME` or `--models-file PATH` (a JSON string
array or one name per line). Coverage/traffic JSON has `rows` containing `name`,
`calls`, and optional `present` (whether imported). Selection takes the top N
traffic rows **before** filtering out `present: false`. Traffic JSON without
`present` selects all names, including direct-provider names that may not be
canonical models. Keep the coverage snapshot fixed for comparable reruns.

Each model gets one non-stream and one stream chat case with 16 output tokens.
`--responses N` adds Responses cases for the first N selected models. Each case
is serialized once and sent unchanged to both sides, sequentially, alternating
A/B then B/A order. An over-budget plan fails before sending anything; it is
not silently truncated. The default cap of 80 counts gateway HTTP requests,
**not** internal upstream attempts. Gateway fallback can multiply those calls.
No retries, redirects, parallel load, warmup, or config changes are performed.
Normal inference can still update a gateway's usage logs and cooldown state.

The new private output directory contains `results.json` and a self-contained
`report.html`. A private `partial.json` checkpoint survives an interrupted run;
reruns must use a new directory. Never copy credential files alongside reports.
Only bounded, redacted error messages and allowlisted routing headers are saved,
not prompt/response bodies. Review artifacts before sharing: redaction is a
defense in depth, not a guarantee against arbitrarily encoded upstream secrets.

Read the report as a functional snapshot, not a gateway speed ranking:

- `match` means both requests succeeded, not identical generated text. Both
  failures remain `both fail`; `same_error_class` distinguishes error parity.
- `llmux-only fail` means only the second gateway failed; `omniroute-only fail`
  means only the first failed. Model verdicts require all its cases to succeed.
  Opposite failures across different cases yield `both fail` for that model.
- HTTP status is preserved. Error classes are heuristics: known local rejection
  codes and 401/403 are `gateway_reject`, 429 is `rate_limited`, other 4xx/5xx are
  `upstream_4xx`/`upstream_5xx`. These names do not establish error provenance.
  Transport or malformed-response failures are `connection`; deadline expiry
  is `timeout`. HTTP 200 containing an error is not success.
- TTFT is dispatch to the first **content** delta, not a role or keepalive event.
  Streams must terminate with `[DONE]`; missing terminators fail even on HTTP
  200. A valid empty-content stream can have no TTFT. Latency includes reading
  the whole JSON response or through `[DONE]`. Total per-request deadlines cover
  stream reads; OS DNS resolution may exceed them, so use IPs for local runs.
- llmux `attempts > 1` proves multiple HTTP attempts. Missing attempts means
  fallback is unknown, not false. OmniRoute routing/fallback headers are shown
  as returned, not interpreted as a complete attempt trace.
- All-result p50 includes errors. Success-only p50 and success TTFT p50 are
  separate; they can compare different successful subsets. One observation per
  case has no repeatability estimate or measured bottleneck. Do not infer a
  speedup from it. For performance decisions, repeat matched successful work
  across multiple trials and establish the noise floor and limiter first.

Operational learnings: OmniRoute's API and dashboard ports may differ; validate
the authenticated `/v1/models` endpoint before a run. Its stored client keys can
be encrypted even though the inference API needs plaintext. Extract/decrypt only
server-side, readonly, into a private file. Imported canonical names can exist
while their upstream IDs still retain custom provider prefixes or inaccessible
container-local addresses; a successful import is not proof of usable routing.

Tests are separate from Rust-only CI (no CI changes):

```sh
python3 -m unittest bench.test_compare_gateways
python3 -m py_compile bench/compare_gateways.py bench/test_compare_gateways.py
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
