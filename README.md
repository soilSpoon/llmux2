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

Before a response is committed, the gateway can fail over on upstream server, connection, and timeout errors within its attempt and time limits. Rate-limit and quota-exhaustion failover are controlled by `routing.fallback_on` and enabled by default. The server uses bounded JSON bodies and SSE events; request timeouts cover upstream setup/first event, then apply as an idle timeout between events. Local RPM uses a bounded per-deployment sliding window only when configured.

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
resource = "https://api.openai.com/v1" # optional; omit scopes for Sign in with ChatGPT
refresh_skew_ms = 60000 # optional; default 60 seconds, zero allowed
request_timeout_ms = 30000 # optional; default 30 seconds
```

`token_url`, `client_id`, and `refresh_token` are required and nonempty. Secrets
must be `env:` or `file:` references, never inline values. Token endpoints require
HTTPS (HTTP is allowed only on loopback), with no userinfo, query or fragment.
Timeouts are 1–86400000 ms; refresh skew is 0–86400000 ms. OAuth startup fetches
the initial access token and fails if that request fails. Access tokens are cached
and refreshed in the background. For `refresh_token = "file:/path/to/token"`, rotations
atomically replace that file (mode 0600 on Unix), so restarts use the latest token.
The directory must be writable; a failed write retains the token in memory but may
require reauthorization after restart. `env:` references are memory-only. Optional
`resource` must be an absolute HTTPS URL without credentials or fragment. For Sign in
with ChatGPT, use `https://auth.openai.com/api/accounts/oauth/token` as `token_url`,
the issued client ID, `resource = "https://api.openai.com/v1"`, and omit `scopes`.
A refreshable authentication error can trigger one on-demand refresh and retry for the attempted deployment, still subject to request and admission limits. If refresh fails or authentication is rejected again, the gateway disables that credential for routing and can try another eligible deployment.

## Development

### Sign in with ChatGPT

Run `cargo run -p llmux-login -- --dir ~/.config/llmux/codex` (the directory is
optional). The command opens the system browser and also prints a private sign-in
URL. Do not share that URL: returning sign-ins include a retained ID-token hint.
The loopback callback uses `127.0.0.1:1455/auth/callback`, falling back to a free port.

After validating the ID token and ChatGPT plan permission, the command atomically
writes owner-only `credentials.json` and `refresh_token` files and prints a TOML
credential snippet. A stable `host_id` survives restarts. Re-running with the same
directory reauthorizes its saved client/account; use a separate directory to add a
different account or workspace. Treat the entire directory as sensitive, never
commit it, and do not run concurrent login/gateway processes for the same session.
The snippet targets `provider = "openai_responses"` and sets
`resource = "https://api.openai.com/v1"`, matching the OAuth configuration schema.
The Responses upstream adapter remains separate work.

See the [engineering standards](AGENTS.md) for test-first development, reliability checks, and PR evidence.

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
