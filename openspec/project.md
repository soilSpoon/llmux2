# llmux2

## Purpose

llmux2 is a Rust API gateway that routes OpenAI-compatible requests across many
authenticated LLM providers. Clients use one canonical model name; the gateway
picks a deployment, converts protocols, and fails over on rate limits and
errors.

The north star is CheaperInference (ci) revenue: the gateway exists to serve ci
traffic at the lowest sustainable unit cost, so capacity, ordering, and quota
decisions all favor revenue earned per unit of quota spent.

## Goals

- Unify OAuth and API-key providers behind one gateway, connecting as many
  providers as practical — free providers and providers the owner already pays
  for alike.
- Map provider-specific model IDs to public canonical model names, keeping ci's
  exposed model ids compatible. Overlapping provider models collapse into one
  canonical model; a request for `A` may be served by same-family variants
  (`A-1`, `A-2`) but never by a different model (`B-1`).
- Support multiple credentials — including several accounts of one provider —
  behind one canonical model. Accounts of a single provider can carry different
  plans, so ordering is per deployment and credential, not per provider.
- Order same-model candidates free → expensive by default, honoring
  quota-expiry-first and remaining-amount scoring and each account's own plan.
- Model provider quota windows explicitly — window start and end, reset time,
  whether a started window is stalled awaiting its first request, and current
  vs. total usage (5-hour, daily, weekly, monthly classes) — and track
  free-model expiry dates, daily caps, and per-minute limits, so allowance is
  spent rather than wasted and exhausted upstreams are parked until reset
  instead of hammered or pruned.
- Detect 429 / quota / failure and fail over to the next deployment.
- Serve CheaperInference traffic (migrated from OmniRoute) with exact
  request/response compatibility.
- Stay reliable under load with minimal overhead: prefer deployments likely to
  hold a warm provider prefix cache, and keep latency, TTFB, and tokens/sec
  optimized.
- Keep the footprint small — memory, CPU, binary size, and build artifacts —
  and keep CI, tests, and builds fast so one dev-loop cycle stays short.

## Non-goals

- Scraping keyless web products: no session-cookie copying, browser
  challenges, or headless TLS-fingerprint tricks (DuckDuckGo, Cloudflare
  Playground — see the providers spec).
- Multi-IP distribution to raise limits.
- OpenRouter as an upstream aggregator (its routed ids are replaced onto
  official endpoints per the migration spec), Factory integrations (expired,
  skipped at import), and paid paths the owner has not asked for. Codex
  (ChatGPT OAuth) is owner-key only.
- Falling back to a larger-context model on context overflow; the gateway
  rejects instead.

## Engineering constraints

- Test-first (TDD), reliability-focused, code quality enforced by CI gates,
  not just written rules.
- CI/CD and tests stay fast: only what is needed, nothing extra.
- Simple, low-overhead, easy to extend and maintain — small binaries and build
  artifacts, low memory and CPU per request.
- Specs are managed with OpenSpec (https://openspec.dev/).
