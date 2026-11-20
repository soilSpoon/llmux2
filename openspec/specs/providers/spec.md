# providers

## Purpose

Which upstream providers llmux2 serves and how they are authenticated.

## Requirements

### Requirement: Unified provider management

The gateway SHALL manage multiple LLM providers behind a single API, each as a
configured deployment with its own credential source.

#### Scenario: Mixed credential types

WHEN two deployments use OAuth (refreshable) and static API keys respectively
THEN both are usable through the same gateway configuration.

### Requirement: Free and usage-limited providers

The gateway SHALL support providers that are free or carry usage limits
(rate limits, periodic quota, expiring quota) alongside paid ones.

#### Scenario: Free-tier provider

WHEN a provider only offers a free tier with a request cap
THEN it is configured as a normal deployment and its limits are tracked like
any other.

### Requirement: Keyless deployments

The gateway SHALL support explicit keyless upstreams via `auth = "none"` with
no credential, for providers that allow anonymous access.

#### Scenario: Anonymous provider

WHEN a deployment is configured with `auth = "none"` and no credential
THEN requests are sent upstream without an authorization header.

### Requirement: Deployment-specific Chat Completions field filtering

The OpenAI Chat adapter SHALL omit configured unsupported fields from the
outbound payload for that deployment in JSON and streaming mode. Unconfigured
deployments SHALL retain their existing request handling.

#### Scenario: Strict OpenAI-compatible deployment

WHEN an OpenAI Chat deployment is configured to strip an unsupported field
THEN only the configured field is dropped, including only the `effort` member
for `reasoning.effort` unless the enclosing object becomes empty.

### Requirement: Static deployment headers

The gateway SHALL support optional per-deployment headers with literal or
env/file-reference values resolved at load time, applied to every upstream
attempt and protected from logging. It SHALL reject invalid, duplicate,
hop-by-hop, framing, and proxy-authentication headers. Authorization and the
adapter credential header SHALL be rejected unless the deployment uses auth none.

#### Scenario: Static secret header

WHEN an anonymous compatible deployment references a secret Authorization file
THEN each attempt sends that header without an adapter-generated credential.

#### Scenario: Unsafe header

WHEN a deployment configures Host or overrides credentialed Authorization
THEN configuration loading fails without exposing its value.

### Requirement: Native Anthropic protocol headers

The gateway SHALL forward client `anthropic-version` and `anthropic-beta`
headers only when passing a native Messages request through to Anthropic.

#### Scenario: Native client selects an API version and betas

WHEN a native Messages request supplies Anthropic protocol headers
THEN the Anthropic upstream receives them, with the gateway's default version
used only if the client omitted a version.

### Requirement: Excluded providers and practices

The gateway SHALL NOT integrate OpenRouter, Factory, multi-account pooling, or
multi-IP distribution to raise limits, and SHALL NOT add paid paths that cost
provider fees unless the owner explicitly asks.

#### Scenario: Codex access

WHEN a Codex deployment uses ChatGPT OAuth
THEN it is reachable only with the owner's own gateway key and never joins a
shared fallback pool.

#### Scenario: xAI OAuth deployment

WHEN a deployment uses an OAuth credential minted by `llmux-login xai`
(the public Grok CLI client on auth.x.ai)
THEN requests go to api.x.ai with the OAuth bearer via `openai_compat`.
