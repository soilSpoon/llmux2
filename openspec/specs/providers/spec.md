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
THEN requests are sent upstream without an adapter-generated authorization header.

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

The gateway SHALL NOT integrate OpenRouter, Factory, Augment, Z.AI,
multi-account pooling, or multi-IP distribution to raise limits, and SHALL NOT
add paid paths that cost provider fees unless the owner explicitly asks.
DuckDuckGo and Cloudflare Playground have no key-based HTTP API and SHALL NOT
be integrated by copying a session cookie, solving a browser challenge, or
driving a headless browser to pass a TLS fingerprint check. The importer SHALL
keep their model ids on an OmniRoute fallback leg when that fallback is
configured, and SHALL otherwise warn that the fallback flags are required.

#### Scenario: Codex access

WHEN a Codex deployment uses ChatGPT OAuth
THEN it is reachable only with the owner's own gateway key and never joins a
shared fallback pool.

#### Scenario: xAI OAuth deployment

WHEN a deployment uses an OAuth credential minted by `llmux-login xai`
or imported from an `xai-oauth` connection
(the public Grok CLI client on auth.x.ai)
THEN requests go to api.x.ai with the OAuth bearer. Chat models use Chat
Completions and the responses-only catalog ids use Responses. A client effort
suffix is stripped before the upstream id.

#### Scenario: Nous Portal subscription deployment

WHEN a deployment uses a `nous_portal` credential created by `llmux-login nous`
(device-code login, rotating refresh token in an owner-only state file)
THEN requests go to inference-api.nousresearch.com with the portal access token
via `openai_compat`, refreshed ahead of expiry and once on a 401, the model is
exposed as `nous/<portal id>`, and it never joins another vendor's combo.

#### Scenario: Default User-Agent on upstream requests

WHEN any adapter (`openai_compat`, `anthropic`, `openai_responses`), the OAuth
and Nous refresh clients, or `llmux-login` sends a request
THEN it carries `User-Agent: llmux/<version>` unless the deployment's `headers`
already set one, because Nous's Cloudflare and Vercel edges answer a request
without a User-Agent with HTTP 403.

#### Scenario: Cloudflare/WAF 403 is not an auth failure

WHEN an `openai_compat` upstream answers 403 with an HTML body
THEN it is classified as a transient server error and the credential stays
enabled; a 403 with a JSON or plain body remains a non-refreshable auth failure
and a 401 remains refreshable.

#### Scenario: Nous login keeps polling through transient portal responses

WHEN `llmux-login nous` polls and the portal answers 403/429/5xx, a non-JSON
body, or fails to respond
THEN it backs off and keeps polling until the device code expires, failing early
only on a definitive OAuth error (`access_denied`, `expired_token`, or another
OAuth `error` code). Errors name the HTTP status and the first 200 characters of
the body, never the device code or tokens.
