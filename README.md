# llmux2

Rust gateway exposing multiple API-key upstreams under one OpenAI-compatible model name. Both `POST /v1/chat/completions` and `POST /v1/responses` support JSON and SSE.

## Run

See [official free API providers](docs/free-providers.md) for importer-gap
replacements, verified free-offer evidence, compatibility caveats, and configs.

1. Copy `examples/llmux.example.toml` to `llmux.toml` and set the upstream base URLs and model IDs. Base URLs include the provider's API prefix, such as `/v1`; the adapter appends `/chat/completions` (or `/responses` for `openai_responses`).
2. Set the secrets named by the config in your environment (or use `file:/path/to/secret` references). The gateway rejects inline keys. Use a separate key for gateway clients.
3. Start the server with `cargo run --locked -- llmux.toml`. It listens on loopback by default; if exposing it remotely, use a TLS reverse proxy.

```sh
curl -sS http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $LLMUX_CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"llama-70b","messages":[{"role":"user","content":"Hello"}]}'
```

For Chat Completions on OpenAI-compatible upstreams, the gateway rewrites only the top-level request model. Other request fields and successful response payloads pass through.

Use authenticated `GET /v1/models` to list the exact model IDs accepted by the gateway. The list contains configured model names and aliases, not upstream deployment names or the full CheaperInference catalog.

```sh
curl -sS http://127.0.0.1:8080/v1/responses \
  -H "Authorization: Bearer $LLMUX_CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"llama-70b","input":"Hello"}'
```

The Responses route translates requests to Chat Completions on OpenAI-compatible upstreams and converts replies back. It accepts reasoning effort as either `reasoning.effort` or `reasoning_effort`; when both are non-null, `reasoning_effort` takes precedence. Stateful Responses conversations and built-in non-function tools are unsupported. For SSE on either route, add `"stream":true` and use `curl -N`.

llmux2 accepts and discards the CheaperInference routing fields `min_discount_percent`, `zdr`, and `ranking`. These fields do not impose a price ceiling, guarantee zero data retention, or change how llmux2 ranks deployments. Enforce any retention requirements in your provider configuration.

Credentials use `provider = "openai"` or `"openai_compat"` for OpenAI-compatible upstreams, or `provider = "anthropic"` for the Anthropic Messages API. Anthropic deployments use a base URL such as `https://api.anthropic.com/v1` (the adapter appends `/messages`) and send the key as `x-api-key`. The gateway converts chat requests, JSON responses and SSE chunks between the OpenAI Chat and Anthropic formats, so a model can mix both kinds of deployments and fail over between them.

Chat requests converted to Anthropic return one choice even when `n` asks for more. For converted Anthropic Chat streams, set `stream_options.include_usage` to `true` for a separate usage chunk just before `[DONE]`, or `false` to omit usage. Without this option, usage remains on the finish chunk. Reported cache reads appear in `usage.prompt_tokens_details.cached_tokens` and cache writes in `usage.cache_creation_input_tokens`.

## Health, shutdown and deployment

`GET /healthz` is unauthenticated and returns `200 ok`; it is excluded from
dashboard statistics. On SIGTERM or SIGINT, the server marks itself draining
and returns 503 only for health probes. For `server.drain_grace_ms` (default
2000, valid range 0–10000 ms), it keeps accepting connections and serving
real requests, adding `Connection: close` to HTTP/1 responses so keep-alive
clients reconnect to the new slot. Zero disables this lame-duck interval.
The 10-second cap stays below the unit's 30-second watchdog: a healthy new
slot's shared-port probe can temporarily reach the draining old slot's 503.
After that interval, it closes the listener and lets in-flight requests and
SSE streams finish. The subsequent drain is bounded by
`server.shutdown_timeout_ms` (default 60000, valid range 1–86400000 ms). It exits
successfully when that timeout expires even if work remains; those unfinished
connections are closed.
Clients should allow streams to finish within that budget.

`llmux-server --check CONFIG` loads and validates configuration without binding
or contacting upstreams, prints the configured listen address, and exits 0
(invalid config: exit 1). Existing configs need no changes. Older binaries do
not recognize `shutdown_timeout_ms` or `drain_grace_ms`; remove unsupported fields
when rolling back to an older version.

Unix listeners use SO_REUSEADDR and SO_REUSEPORT for overlapping slot handoffs.
Under systemd, NOTIFY_SOCKET receives READY=1 after application/listener setup
and STOPPING=1 at drain start. WATCHDOG_USEC enables a periodic TCP HTTP health
probe; only a successful HTTP 200 sends WATCHDOG=1. See the
[user-systemd deployment guide](deploy/README.md) for install, recovery,
migration and rollback instructions, including the host's TCP queue migration
sysctl.

## Live dashboard

Add `dashboard_key = "env:LLMUX_DASHBOARD_KEY"` to `[server]`, set that
environment variable to a strong key distinct from the gateway client keys,
and restart llmux2. Open `http://127.0.0.1:8080/dashboard` (or the configured
gateway address) and enter the dashboard key. Use TLS when accessing it
remotely. The page keeps the key in memory until you lock or close it.
The dashboard UI is in Korean. It defaults to a light theme and remembers
the theme you choose in this browser; the dashboard key is never stored.

The dashboard shows response rate, status counts, mean time to response
(first upstream event for streams), and routed requests, failed attempts,
stream errors and cooldowns per deployment. In-flight counts are shown
where a concurrency limit is configured. A 429 that successfully fails
over appears as a failed attempt, not a gateway 429. Gateway counts cover
authorized requests to the chat and Responses endpoints. They count
responses when ready, not when downstream transfer finishes.

Metrics stay in memory on each instance and reset on restart. The chart
starts collecting when opened; it polls every two seconds. The dashboard
does not collect prompts, responses, upstream URLs or credential values
in its stats. Omit `dashboard_key` to disable the routes and collection.

## Routing

Lower numeric priority wins, with round-robin ordering among equal priorities. On an upstream HTTP 429, the gateway tries the next deployment and skips the limited deployment until its `Retry-After` (seconds or HTTP date) expires. Missing values use the configured default, doubled for each consecutive 429 from that deployment until it serves a request again, and waits are capped by `cooldown.max_ms`. A 429 or quota error that arrives after a stream has started cannot be retried, but it still cools the deployment down for later requests. A response whose rate-limit headers report zero remaining requests or tokens also skips that deployment until the reported reset. After a 429 or quota cooldown ends, the deployment takes one request at a time until one succeeds, and other requests fall back meanwhile, so a still-limited provider is not hit by every queued request at once. If none can serve the request, the gateway returns 429 with the earliest known retry delay. Optional local per-deployment request (`quota.rpm`, `quota.rpd`), estimated-token (`quota.tpm`, `quota.tpd`) and concurrency limits also skip deployments without contacting them. Deployments under different models that share the same provider, credential, base URL, upstream model and quota share one cooldown and one set of local limits, so a 429 on one model's route is not immediately retried through another model.

Each deployment may set `max_context` (prompt plus completion tokens), defaulting to the model's `capabilities.max_context`. Before routing, the gateway estimates prompt tokens (about 4 bytes of prompt text per token, ignoring base64 `data:` URLs) plus the requested output limit (`max_output_tokens` for Responses, `max_completion_tokens` else `max_tokens` for Chat), and skips deployments of that model that it would not fit. It never falls back to a different model: if no deployment fits, it returns 400 `context_length_exceeded`, naming the limit, without contacting upstream.

Rejecting is the default. To trim instead, set `context_overflow = "middle-out"` on a model, or send `x-llmux-context-overflow: middle-out` (or `reject`) per request. The gateway then drops messages (Responses `input` items) from the middle of the conversation until the request fits the largest window. It keeps system and developer messages and the latest message, and drops tool results together with the call before them. The response carries `x-llmux-dropped-messages: <n>`, so a trim is never silent. If dropping every middle message still doesn't fit, the request gets the same 400.

`x-llmux-deployment` and `x-llmux-attempts` identify the winning deployment and number of HTTP attempts (not skipped deployments). Set `server.expose_routing_headers = false` to hide them. A stream commits on its first complete SSE event; after that, failures end the stream with an error payload rather than replaying the request.

Before a response is committed, the gateway can fail over on upstream server, connection, and timeout errors within its attempt and time limits. Rate-limit and quota-exhaustion failover are controlled by `routing.fallback_on` and enabled by default. The server uses bounded JSON bodies and SSE events; request timeouts cover upstream setup/first event, then apply as an idle timeout between events. Local request and token quotas use per-deployment sliding windows (one minute for `rpm`/`tpm`, one day for `rpd`/`tpd`) only when configured. Token quotas count the same prompt-plus-output estimate used for context fitting, and a single request larger than a token quota is still admitted when that window is empty. A credential may also set `quota` (`rpm`, `rpd`, `tpm`, `tpd`) for providers that limit per key or project across models; those windows are shared by every deployment that uses the credential.

## Credentials

OpenAI-compatible deployments can explicitly opt into anonymous access with
`auth = "none"` and `provider = "openai_compat"`, omitting `credential`. They
send no upstream Authorization header. Omitting a credential without this opt-in,
or combining it with `auth = "none"`, is a configuration error. Keyed deployments
keep their existing `credential = "id"` spelling. A model can mix both forms,
with independent deployment limits and cooldowns; see the
[mixed Kilo example](docs/free-providers.md#kilo-gateway-mixed-anonymous-and-keyed).
Use anonymous access only when the upstream officially documents it. Multiple
anonymous deployments do not multiply a provider's per-IP quota.

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

## Codex / Sign in with ChatGPT

`provider = "openai_responses"` calls the public Responses endpoint with an OAuth
bearer, never a ChatGPT backend endpoint. Use it **only for the owner's own account
and requests**, not to share or resell ChatGPT plan capacity to gateway users.
OpenAI describes this flow for [open-source and locally hosted apps](https://developers.openai.com/siwc/token-sharing-open-source);
review the [preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations).
The gateway's owner-only restriction is an additional project policy.

Use the credential block printed by `llmux-login`, then add a dedicated model and
its owner-key restriction as below. Replace the issued client ID and absolute
refresh-token path with the values from your login. The default upstream base URL
is `https://api.openai.com/v1`; choose an upstream model available to your account.
Do not run login concurrently with a gateway using the same rotating token file.

```toml
[server]
client_keys = ["env:LLMUX_OWNER_KEY", "env:LLMUX_OTHER_KEY"]

[[credentials]]
id = "codex"
kind = "oauth"
provider = "openai_responses"
token_url = "https://auth.openai.com/api/accounts/oauth/token"
client_id = "REPLACE_WITH_ISSUED_CLIENT_ID"
refresh_token = "file:/absolute/path/to/.config/llmux/codex/refresh_token"
resource = "https://api.openai.com/v1"

[[models]]
name = "owner-codex"
allowed_keys = ["env:LLMUX_OWNER_KEY"]

[[models.deployments]]
id = "owner-codex-primary"
credential = "codex"
upstream_model = "REPLACE_WITH_ACCOUNT_MODEL_SLUG"
# base_url = "https://api.openai.com/v1" # optional for this provider
```

`models.allowed_keys` is a nonempty list of `env:`/`file:` secret references whose
resolved values must already be in `server.client_keys`. It applies to the whole
canonical model, all aliases, and every deployment/fallback candidate. A different
authenticated gateway key gets 403 before routing or contacting an upstream.
Every model containing an OAuth `openai_responses` deployment **must** declare
this restriction, even if that deployment is only a backup. Models without it
remain available to all authenticated keys; routing never falls back across models.
Do not put a Codex credential in a model intended for other users.

Both `/v1/responses` and `/v1/chat/completions` are supported, with JSON or SSE
downstream. Upstream always uses `store: false`, `stream: true`, and an input array;
non-streaming callers receive the final `response.completed` object (or converted
chat completion), including usage. Chat system messages become `instructions`.
Unsupported preview fields (including output-token limits and sampling controls)
are omitted, so they do not constrain upstream generation. Stateful conversation
references, explicit Responses system items, and unsupported hosted tools are
rejected with 400. An incomplete, failed, or interrupted stream is not successful;
after SSE commitment errors terminate the stream without replaying the request.
Anthropic inbound is not supported by this adapter.

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
node --test crates/llmux-server/tests/dashboard.cjs
```

In CI, `coverage` runs the Rust unit and integration tests once with coverage
instrumentation, while `check` runs Rust doctests, lint, and the dashboard and
repository tooling tests. Use `cargo test --locked --workspace` to run all Rust
tests locally without instrumentation.

## Live gateway comparison

Run on the gateway host with Python 3.9+ (stdlib only), reference first, llmux second.
Keep keys server-side: use `file:/absolute/path` or `env:VARIABLE`, never literal keys.

```sh
python3 bench/compare_gateways.py \
  --gateway omniroute http://127.0.0.1:20128/v1 file:/private/omniroute-client.key \
  --gateway llmux http://127.0.0.1:8090/v1 file:/private/llmux-client.key \
  --coverage /private/coverage.json --top 25 --responses 2 \
  --timeout 45 --max-requests 80 --output /private/compare-run-001
```

Or use repeated `--model NAME` / `--models-file PATH` (JSON array or one name per line).
Coverage selects top-N traffic rows, then filters `present: false`; keep the snapshot fixed.
Identical chat/stream bodies alternate A/B order; `--responses N` adds N Responses cases.
The cap is checked before sending and counts HTTP requests, not internal fallback attempts.
Read `results.json` / `report.html` in a new private output directory; review before sharing.
- `match`: both succeed, not identical text; `llmux-only fail` / `omniroute-only fail`: only that side fails.
- `both fail`: neither side succeeds in all cases; model verdicts require every case to succeed.
- Single sample, not a speed ranking: all-result p50 includes errors; successful subsets can differ.
- TTFT measures first content; streams require `[DONE]`. HTTP 200 with an error is not success.
- Missing llmux attempts means fallback unknown ≠ false; attempts >1 proves multiple attempts.
- Routing headers are evidence, not a complete trace; error classes do not prove error provenance.
Learnings: API and dashboard ports differ—validate authenticated `/v1/models` first.
Stored client keys may be encrypted; extract/decrypt readonly, server-side, into private files.
Imported names can retain wrong upstream IDs or container-local URLs: import success ≠ usable routing.
Run just the comparison harness tests with `python3 -m unittest bench.test_compare_gateways`.
Check Python syntax with `python3 -m py_compile bench/compare_gateways.py bench/test_compare_gateways.py`.

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
