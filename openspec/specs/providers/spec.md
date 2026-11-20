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
