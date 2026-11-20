# llmux2

## Purpose

llmux2 is a Rust API gateway that routes OpenAI-compatible requests across many
authenticated LLM providers. Clients use one canonical model name; the gateway
picks a deployment, converts protocols, and fails over on rate limits and
errors.

## Goals

- Unify OAuth and API-key providers behind one gateway.
- Map provider-specific model IDs to public canonical model names.
- Use free and usage-limited providers alongside paid ones, routed by priority.
- Detect 429 / quota / failure and fail over to the next deployment.
- Stay reliable under load with minimal overhead.
- Serve CheaperInference traffic (migrated from OmniRoute) with exact
  request/response compatibility.

## Non-goals

- Factory integrations, OpenRouter, multi-account or multi-IP pooling, and
  paid paths that add provider cost. Codex (ChatGPT OAuth) is owner-key only.
- Falling back to a larger-context model on context overflow; the gateway
  rejects instead.

## Engineering constraints

- Test-first (TDD), reliability-focused, code quality enforced by CI gates,
  not just written rules.
- CI/CD and tests stay fast: only what is needed, nothing extra.
- Simple, low-overhead, easy to extend and maintain.
- Specs are managed with OpenSpec (https://openspec.dev/).
