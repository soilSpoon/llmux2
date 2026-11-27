# cheaperinference-migration

## Purpose

Moving CheaperInference traffic from OmniRoute onto llmux2.

## Requirements

### Requirement: Import native context-window overrides

The importer SHALL map valid model_context_overrides rows, and their camel-case
export equivalents, to the existing native deployment max_context field using
the routing provider identity and exact upstream model ID. Token counts SHALL
be positive u32 integers. Invalid and duplicate rows SHALL warn without exposing
their values. Reports SHALL count applied, unmatched, and invalid rows separately.
Unsupported capability override rows SHALL be counted and SHALL NOT be treated
as total context windows. No new output-token clamp or model-list metadata SHALL
be introduced by this import behavior.

#### Scenario: Mixed native combo and aliases

WHEN native combo members have different context overrides
THEN direct, nested, and aliased routes retain each deployment's own limit
AND existing context-fit routing uses those limits.

#### Scenario: Unimported target or unknown fallback capacity

WHEN an override targets no imported native deployment
THEN it is counted as ignored and does not create a route
AND OmniRoute fallback deployments do not inherit native context overrides.

#### Scenario: Missing or invalid overrides

WHEN the override table is absent or empty
THEN existing deployment limits remain unchanged
WHEN a row has an invalid token count
THEN the row is skipped with a warning.

### Requirement: Reviewed Antigravity Gemini replacement

The importer SHALL map antigravity/gemini-3-flash to
gemini-3-flash-preview and antigravity/gemini-3.1-flash-lite to the GA
gemini-3.1-flash-lite on all eligible built-in Gemini API-key connections in
priority order, preserving aliases and combo references. Source Antigravity
connection restrictions SHALL be replaced by the eligible Gemini set with a
warning. An Antigravity OAuth connection that lacks a refresh token or a
project id SHALL remain skipped; when the export omits the OAuth client pair
the importer SHALL use the public client id and secret the Antigravity CLI
embeds (OmniRoute stores only per-connection tokens). A complete connection
SHALL be imported
as the native `antigravity` provider and SHALL serve the requested model id,
not a rewritten Gemini id. The reviewed Gemini rewrite SHALL apply only when
no Antigravity OAuth connection was imported. Ordinary OmniRoute
fallback semantics SHALL apply after native deployments when enabled.
No other Antigravity models SHALL be mapped. Both reviewed routes SHALL receive
the Gemini endpoint's seed/user request-field stripping.

#### Scenario: Reviewed Flash route

WHEN a combo or alias resolves to antigravity/gemini-3-flash
THEN it uses Gemini deployments followed by the optional OmniRoute fallback.

#### Scenario: OCI-verified GA Flash Lite route

WHEN a combo or alias resolves to antigravity/gemini-3.1-flash-lite
THEN it uses gemini-3.1-flash-lite on eligible Gemini connections in priority
order, strips seed/user, and appends the optional OmniRoute fallback last.
OCI verified all five keys list the GA model and generateContent returns 200
with modelVersion gemini-3.1-flash-lite.

#### Scenario: No eligible Gemini connection

WHEN no eligible Gemini API-key connection exists
THEN no native replacement deployment is emitted and a warning explains why.

### Requirement: Import live OAuth and keyed loopback bridges

The importer SHALL import `xai-oauth`/`xao` as an `xai` OAuth credential
refreshing at `https://auth.x.ai/oauth2/token` with deployments at
`https://api.x.ai/v1`. It SHALL import `codex`/`cx` as `openai_responses` at
`https://chatgpt.com/backend-api/codex`, refreshing at
`https://auth.openai.com/oauth/token`, and SHALL restrict those models to the
imported gateway keys. It SHALL import a complete Antigravity OAuth connection
at `https://daily-cloudcode-pa.googleapis.com` without rewriting the model id.
A missing refresh token or client id SHALL skip that row with a counted
warning and SHALL NOT panic, except for Antigravity where the public embedded
client id and secret are the default when the export omits them and a missing
project id still skips the row. The importer SHALL NOT invent a client secret:
export-supplied secrets win and remain secrets-file references, and only the
upstream-published Antigravity pair may fill an empty field.
A keyed OpenAI-compatible `deepai`/`deepai-code` or `cloudflare-direct`/`cfpd`
node with a baseUrl SHALL import as `openai_compat` at that baseUrl, including
loopback. DuckDuckGo and Cloudflare Playground have no native adapter. Without
`--omniroute-fallback` and `--omniroute-key`, their skip warning SHALL name
those flags. With both flags, each of their model ids SHALL get an OmniRoute
fallback deployment last, with that same upstream model id. A session model id
attached to an imported model as an alias SHALL resolve to that model at
request time and SHALL NOT get its own model or fallback leg, so a canonical
request never sends the session alias upstream. Factory, Augment, and Z.AI
SHALL remain excluded.

#### Scenario: Session id attached as an alias

WHEN a session provider model id is attached to an imported model's aliases
THEN the model's last OmniRoute fallback deployment keeps the imported model
name as its upstream model id
AND no extra model or fallback leg is created for the alias.

#### Scenario: xAI OAuth token URL

WHEN an `xai-oauth` connection has a refresh token and client id
THEN the generated credential refreshes at `https://auth.x.ai/oauth2/token`
AND the deployment base URL is `https://api.x.ai/v1`
AND the refresh token is not written into the configuration or warnings.

#### Scenario: Missing refresh token

WHEN an `xai-oauth` connection has no refresh token
THEN import skips that row with a missing-refresh-token warning and continues.

#### Scenario: DeepAI Code loopback

WHEN a DeepAI Code node has a valid API key and a loopback `*/deepai/v1` base
URL
THEN its deployment uses `openai_compat` at that URL and the same model id.

#### Scenario: Antigravity connection without a stored client pair

WHEN an `antigravity` connection has a refresh token and project id but no
client id or client secret
THEN the generated OAuth credential uses the public embedded Antigravity
client pair, the refresh token is written to a secrets file, and its
deployments carry the connection's `x-llmux-project` header.

### Requirement: Native Gemini and Antigravity connection mapping

An active, usable `gemini` connection with `auth_type = apikey` and a
plaintext key SHALL produce an `api_key` credential whose key is written to a
secrets file, and every usable combo step with `providerId = "gemini"` SHALL
emit a native `openai_compat` deployment at
`https://generativelanguage.googleapis.com/v1beta/openai` serving the step's
own model id on an eligible imported Gemini connection. Likewise, every
usable combo step with `providerId = "antigravity"` SHALL emit a native
`antigravity` deployment at `https://daily-cloudcode-pa.googleapis.com` on
each imported Antigravity connection. With N imported connections of one
provider, each step SHALL produce one deployment per connection ordered by
the connections' priorities, and an enabled OmniRoute fallback SHALL remain
after every native leg.

#### Scenario: Multi-account step order

WHEN a combo step names `gemini` or `antigravity` and two connections of that
provider imported in priority order
THEN the model carries both native deployments in that order before any
OmniRoute fallback leg.

### Requirement: Custom provider headers

The importer SHALL preserve compatible node custom_headers_json/customHeaders
as static deployment headers, replacing the node map with a connection override
when present. It SHALL write every value to a private 0600 file and emit only
file references. Invalid or forbidden headers SHALL skip the connection with
a warning that does not expose values.

#### Scenario: HMS node

WHEN an eligible custom node has valid custom headers
THEN its combos and direct routes load with those headers on each deployment.

#### Scenario: Empty override

WHEN a connection has an explicitly empty customHeaders map
THEN no node headers are inherited.

### Requirement: DeepSeek parameter compatibility

The importer SHALL configure openai and openai_compat adapter deployments with
DeepSeek upstream model IDs to strip seed, user, safety_identifier, and
reasoning.effort through the core
strip_request_fields option. It SHALL preserve logprobs and top_logprobs, and
leave non-DeepSeek model deployments unchanged. Classification SHALL be
case-insensitive after routing-prefix stripping: a nonempty deepseek-* final
model segment or an exact deepseek or
deepseek-ai vendor segment. The rule SHALL be independent of connection provider
and public model name, and SHALL include matching fallback upstream IDs.

#### Scenario: DeepSeek combo or direct route

WHEN a route expands onto a DeepSeek upstream model on any connection
THEN the generated deployment loads with exactly those four stripped fields.

#### Scenario: Non-DeepSeek model or substring lookalike

WHEN the upstream ID is not a DeepSeek model under the matching rule
THEN no DeepSeek field stripping is added, even on a DeepSeek provider connection.

#### Scenario: DeepSeek model on an unsupported adapter

WHEN a DeepSeek upstream model uses an anthropic-compatible node
THEN the deployment imports successfully without strip_request_fields.

### Requirement: Anthropic sampling parameter compatibility

The importer SHALL set `strip_request_fields = ["temperature", "top_p"]` on
anthropic adapter deployments whose upstream model leaf, with `.` and `-`
treated alike and case ignored, is `claude-<family>-5…` (for example
`claude-sonnet-5-5`, `claude-haiku-5.5`, `claude-fable-5-1`) or `claude-opus-4-8…`.
Older Claude models, such as `claude-sonnet-4-6`, SHALL import unchanged.

#### Scenario: Claude 5 deployment

WHEN a combo expands onto `anthropic/claude-sonnet-5-5`
THEN the generated deployment loads with `temperature` and `top_p` stripped.

#### Scenario: Older Claude model

WHEN the upstream model is `claude-sonnet-4-6`
THEN no field stripping is added.

### Requirement: Gateway self-referential connections are skipped

An OmniRoute connection whose resolved upstream URL targets this gateway —
a container-runtime host alias (`host.containers.internal`,
`host.docker.internal`, `gateway.docker.internal`) on the emitted listen
port — SHALL be skipped with a warning, because it is the OmniRoute-to-llmux2
bridge and importing it would loop requests into llmux2 itself. The check
SHALL use the port the emitted config actually binds: the `--listen` port,
else the site overlay's `[server].listen` port, else 8080. Host aliases on
other ports SHALL import normally.

#### Scenario: Bridge node on the listen port

WHEN a node baseUrl is `http://host.containers.internal:8090/v1` and the
imported listen is `127.0.0.1:8090`
THEN the connection is skipped with a gateway warning, its combo steps report
no eligible connection, and import succeeds.

#### Scenario: Bridge node on an overlay-only listen port

WHEN a node baseUrl is `http://host.containers.internal:8090/v1`, no
`--listen` is passed, and the site overlay sets `server.listen` to
`127.0.0.1:8090`
THEN the connection is skipped with a gateway warning.

#### Scenario: Same alias on another port

WHEN a node baseUrl uses a gateway host alias on a port other than the
emitted listen port
THEN the connection is not treated as self-referential.

### Requirement: Reviewed Morph official migration and logprobs order
The importer SHALL migrate only built-in Morph loopback connection overrides to
`https://api.morphllm.com/v1`, warning about the reviewed endpoint substitution.
Other loopback providers, custom nodes, nonofficial Morph hosts, and unspecified
bind addresses SHALL remain excluded except for the reviewed DeepAI Code sidecar
below. The existing Morph credential is reused.

The reviewed combo names `deepseek-v4.1-flash`, `deepseek-v4.1-flash-fast`,
`deepseek-v4.1-flash-1017`, `deepseek-v4.1-flash-1017-fast` and
`deepseek-v4.1-flash-1017-ultrafast` SHALL place official Morph
`morph-dsv41flash-1017` deployments before Vibe/Mistral
`deepseek-v4-1-flash-1017` deployments and bridge replacements. The three fast
combos SHALL start with Morph; the two base combos SHALL retain declared Numes
ahead of Morph. Remaining legs SHALL preserve their relative order.
This explicit policy follows OCI verification of Morph models.list and chat
logprobs, and Mistral's rejection of logprobs for this model; it SHALL NOT strip
logprobs, reorder unrelated combos, or replace the optional final fallback.

#### Scenario: Reviewed loopback Morph leg and Mistral leg
- **WHEN** a reviewed combo contains both verified legs
- **THEN** the official Morph leg precedes Mistral and a migration warning is emitted

#### Scenario: Unreviewed model or combo
- **WHEN** the Morph model or combo name is not reviewed, or either leg is absent
- **THEN** no reviewed logprobs-order policy is applied

### Requirement: Reviewed free XPLabs routes first
For the reviewed source combos `gpt-6-luna`, `deepseek-v4.1-flash` and
`deepseek-v4.1-flash-fast`, the
importer SHALL place one deployment of the matching Experiential Labs (XPLabs)
model (`gpt-6-luna` or `deepseek-v4.1-flash`) first, pinned to an active, usable
API-key connection of the custom `experlabs` node at
`https://api.experientiallabs.ai/v1`. In the fast
combo it SHALL follow the verified Morph route. A declared leg for the same model
and endpoint SHALL be moved instead of duplicated, keeping its pinned
connection, other legs SHALL keep their
relative order, and a warning SHALL be emitted when the order changes. This
follows OCI verification that these models cost 0 on the existing key and that
XPLabs DeepSeek returns logprobs. The earlier
`deepseek-v4.1-flash-uncensored-fast:free` promo leg was removed after it went
audience-gated (`model_not_granted` on the key, 2026-11-25); a promo slug that
reappears SHALL be re-verified before rejoining the set. Every XPLabs
deployment whose model is not in
the verified-free set SHALL be dropped with a warning, in every combo and
strategy: the account is overdrawn, so billed models OCI observed
(`claude-haiku-5.5`, `qwen3.8-27b`, `deepseek-v4.1-flash-1017`) must never be
generated, and a combo left with no deployments is skipped. The verified-free
set SHALL be checked against a committed snapshot of the keyless
`GET /api/plan-discounts` endpoint: every emitted slug must still reach a 100%
discount on some paid plan.

#### Scenario: Reviewed combo with an eligible XPLabs connection
- **WHEN** a reviewed combo has paid legs and an eligible XPLabs connection exists
- **THEN** the XPLabs model is priority 0 and the paid legs follow in order

#### Scenario: Billed XPLabs leg
- **WHEN** any combo contains an XPLabs leg whose model is not verified free
- **THEN** that leg is dropped with a warning and other legs keep their order

#### Scenario: Ineligible connection, unreviewed combo or round-robin
- **WHEN** the XPLabs connection is inactive, unusable, OAuth, on another host or
  prefix, or the combo is unreviewed
- **THEN** no free route is placed, while billed legs are still dropped

### Requirement: Reviewed secondary routes
With `--openrouter-replacements`, for the existing source combo `gpt-6-luna`, the importer SHALL append the
ordered upstream IDs `gpt-6-luna`, `openai/gpt-6-luna`, and
`perplexity/gpt-6-luna` after surviving source legs, using one active, usable
API-key custom openai_compat connection whose node prefix is `respan` and whose
normalized endpoint is exactly `https://api.respan.ai/api/v1`. This reviewed
tuple follows OCI verification of Responses JSON and SSE. For the existing
source combo `kimi-k3.1-pro`, the importer SHALL append `morph-kimik31` after
surviving source legs using one active, usable API-key builtin `morph`
connection whose endpoint is exactly `https://api.morphllm.com/v1`; a builtin
reviewed secondary needs no provider node, and free-first routing keeps the
declared numes leg primary. This reviewed tuple follows OCI verification that
the pod-internal `cfpd` sidecar is unreachable from the llmux2 host and that
Morph serves the Kimi traffic. Azure OpenAI, Azure
Foundry and Bedrock variants SHALL NOT be added by this policy.
The importer SHALL use the first eligible connection in existing priority
order, skip each upstream ID already deployed on that endpoint, preserve source
leg order and assign consecutive priorities before optional OmniRoute fallback.
It SHALL NOT use Codex, OAuth or anonymous connections, synthesize a primary for
an empty/missing combo, or augment unrelated combos. Round-robin combos SHALL
remain unchanged with a warning. Normal upstream endpoint normalization SHALL
apply, including connection overrides; generated config and warnings SHALL NOT
contain credential values. Removing an ID or the reviewed row and regenerating
config SHALL roll back that part or all of this policy.

#### Scenario: Reviewed secondary follows Experlabs
- **WHEN** a non-round-robin Luna combo has surviving legs and an eligible Respan connection
- **THEN** the missing reviewed IDs are appended in declared order on one connection, before fallback

#### Scenario: Unavailable or already declared route
- **WHEN** the reviewed connection is unavailable or an ID already exists on the reviewed endpoint
- **THEN** no unavailable or duplicate deployment is added

#### Scenario: Opt-in flag absent
- **WHEN** `--openrouter-replacements` is absent, even with an eligible reviewed connection
- **THEN** no secondary route is appended and the combo's source legs are unchanged

### Requirement: Gemini OpenAI endpoint parameter compatibility

The importer SHALL add seed and user stripping to openai and openai_compat
deployments whose parsed upstream host is exactly generativelanguage.googleapis.com,
case-insensitively. It SHALL preserve other fields unless another reviewed rule
already strips them, and SHALL NOT infer endpoint identity from model names.

#### Scenario: Gemini endpoint on a custom node

WHEN a compatible custom node uses the Google Gemini endpoint
THEN its deployments load with seed and user stripped regardless of the node ID.

#### Scenario: Another host or hostname lookalike

WHEN a Gemini-named model uses another host or a hostname containing the Google host as a substring
THEN no Gemini stripping is added.

### Requirement: Exact API compatibility

The gateway SHALL match the request arguments and response shapes
CheaperInference requires, exactly.

#### Scenario: Existing client unchanged

WHEN a CheaperInference client switches its endpoint from OmniRoute to llmux2
THEN its requests and parsed responses work without code changes.

### Requirement: Complete migration before switchover

All OmniRoute traffic SHALL be migrated, with nothing dropped, before
llmux2 takes over; real OmniRoute traffic is the reference for coverage.

#### Scenario: Coverage checked against OmniRoute logs

WHEN comparing OmniRoute's routed models against llmux2's coverage
THEN every model and request shape OmniRoute serves is mapped before the
switch.

### Requirement: Optional OmniRoute cutover fallback

The importer SHALL accept `--omniroute-fallback` and `--omniroute-key` together
or neither. The key SHALL be a nonempty `env:` or `file:` reference and SHALL
remain a reference in generated TOML. The URL SHALL obey gateway base URL
validation: HTTPS, or HTTP only for loopback, without credentials, query, or
fragment. Generated configuration SHALL pass the existing runtime config parser.
Without these flags, existing import output SHALL remain byte-identical.

#### Scenario: Fallback leg is last

WHEN fallback flags are provided for an unrestricted non-Codex imported model
THEN its OmniRoute deployment has priority strictly greater than every native
deployment, a unique ID, and an upstream model equal to the llmux model name.

#### Scenario: Codex and allowed-key models get no fallback leg

WHEN an imported model can reach Codex or has `allowed_keys`
THEN the importer adds no OmniRoute deployment to that model.

#### Scenario: Codex IDs excluded from catch-all

WHEN fallback flags are provided
THEN the importer emits a `*` model with `upstream_model = "*"`
AND excludes Codex provider aliases and node prefixes, every Codex-reaching
combo and model alias, enabled mapping patterns that can reach Codex, and the
names and aliases of every allowed-key model, even if Codex connections were
not imported.

#### Scenario: Unsafe mapping cannot be expressed

WHEN an enabled Codex-routing mapping pattern cannot be expressed as an exact
ID or a prefix with a single trailing `*`
THEN import fails clearly without writing the output configuration.

### Requirement: Fallback policy and audit artifacts are typed and redacted

The importer library SHALL parse `fallback-policy.json` as a schema-versioned
typed artifact before any policy-mode use. Schema version 1 SHALL define
fallback decisions `suppress`, `retain`, `transition-retain`, and
`hold-existing-unresolved`; spelling actions `alias`, `drop`,
`retain-catchall`, `needs-review`, and `already-resolved`; and catch-all modes
`transition` and `strict`. Every model, spelling, and catch-all decision SHALL
carry provenance. `automatic-rule` provenance SHALL name its rule, and
`user-approved` provenance SHALL name a non-secret `approval_source`.

The artifact schema SHALL have no credential, client-key, token, or secret
fields and SHALL reject unknown fields. Validation SHALL fail closed rather
than emit recognizable secret references or tokens in free text. The
`fallback-audit.json` writer SHALL produce deterministic bytes for equivalent
input by sorting decisions and native-leg ids, and its summary SHALL agree
with the decision rows. Needs-verification and held-unresolved rows count
toward `needs_review` until the schema grows separate counters.

This typed artifact support SHALL NOT by itself change importer output,
fallback emission, routing, or production behavior. Policy-mode importer flags
and runtime application are later migration phases.

#### Scenario: Unresolved states are representable

WHEN a schema version 1 policy uses `hold-existing-unresolved` for a model
and `needs-review` or `already-resolved` for a spelling
THEN typed parsing accepts those states without treating them as user approval.

#### Scenario: User approval needs evidence

WHEN a decision has `decision_source = "user-approved"` without
`approval_source`
THEN schema validation fails.

#### Scenario: Audit output is deterministic and redacted

WHEN equivalent audit decisions are serialized in different input orders
THEN the bytes are identical
AND secret references or credential and client-key metadata fail validation
instead of appearing in output.

### Requirement: Restricted client keys keep their model policy

An export API key SHALL be imported as a `client_keys` entry when its only
restrictions are `model_access_mode = restricted` plus `allowed_models` and
`blocked_models` glob lists, and the importer SHALL emit a matching
`server.key_policies` entry with `models` from `allowed_models` and `blocked`
from `blocked_models`. A key carrying any other restricted field SHALL stay
skipped — importing it unrestricted would silently widen access — and a
restricted key with an empty `allowed_models` list SHALL stay skipped because
it denies every model.

#### Scenario: Model-list restriction is ported

WHEN an export key is `restricted` with `allowed_models = ["cfpd/*", "m"]`
and `blocked_models = ["zc/*"]`
THEN the emitted config authenticates the key and binds it to a
`key_policies` entry with `models = ["cfpd/*", "m"]` and
`blocked = ["zc/*"]`.

#### Scenario: Untranslatable restriction stays closed

WHEN an export key combines model lists with a restriction llmux2 cannot
express, such as `ip_allowlist` or `rate_limits`
THEN the key is skipped with a warning and no secret file is written for it.

### Requirement: SQLite dump key fields normalize before the port

API key rows from a SQLite table dump SHALL have their JSON-encoded text
columns (`allowed_models`, `blocked_models`, `allowed_combos`,
`allowed_connections`, `allowed_quotas`, `allowed_endpoints`,
`ip_allowlist`, `rate_limits`, `scopes`, `preferred_connections`) parsed
into arrays during normalization so dump-form and export-form rows behave
identically downstream.

A restricted key that also carries OmniRoute-internal fields — `scopes`,
`allowedCombos`, `allowedConnections`, `catalogScope`, `machineId` — SHALL
still port its model lists; the importer drops those fields and emits a
warning naming each dropped field. A restricted field outside that list
(ip allowlists, quotas, rate limits, schedules, forced models) keeps the
fail-closed skip.

#### Scenario: Dump row with internal fields still ports

WHEN a dump `api_keys` row is `restricted` with string-encoded
`allowed_models`/`blocked_models` and non-empty `scopes`,
`allowed_combos`, `allowed_connections`, `catalog_scope`, `machine_id`
THEN the key imports, `server.key_policies` carries the parsed model
lists, and a warning names the dropped fields.

#### Scenario: Real restriction outside the internal list stays closed

WHEN a dump `api_keys` row is `restricted` with a `rate_limits` entry
THEN the key is skipped with a warning and no secret file is written.

### Requirement: Revenue-oriented allocation

Provider and capacity allocation SHALL favor increasing CheaperInference
revenue.

#### Scenario: Capacity added where it earns

WHEN choosing between two providers to expand
THEN the one serving higher-revenue models is preferred.

### Requirement: No OmniRoute investment

Work SHALL go into the llmux2 replacement, not into improving the OmniRoute
server itself.

#### Scenario: OmniRoute bugs not fixed

WHEN an OmniRoute defect is found during migration
THEN it is worked around on the llmux2 side rather than patched upstream.

### Requirement: Explicit Workers AI import

The importer SHALL require both `--cloudflare-account-id-file` and
`--cloudflare-token-file` (or their API equivalents) before replacing
Cloudflare bridge legs with official Workers AI deployments. It SHALL read
only the non-secret, validated account ID to construct the account URL and
SHALL reference the token as an absolute `file:` credential without opening
or copying it, including during real-loader validation.

#### Scenario: Registry aliases share canonical connections

WHEN opted-in legs use `cfpd` or `cfp`
THEN they use eligible `cloudflare-direct` or `cloudflare-playground`
connections respectively, with reviewed exact `@cf/` upstream IDs.

#### Scenario: Unsupported or unapproved Workers AI route

WHEN opt-in is absent or a model is unknown, deprecated, or excluded
THEN no Workers AI deployment is emitted for that leg and a warning explains
the omission, including for `@cf/meta/llama-3.2-1b-instruct`.

#### Scenario: Workers AI quota is not a spending guarantee

WHEN a Workers AI deployment is emitted
THEN it has `rpm = 30` (configurable with `--workers-ai-rpm`, which requires
the Cloudflare opt-in), and the shared Workers AI credential has
`tpd = 6500`, a hard daily cap on estimated input tokens bound across every
deployment on the account, sized so a day of admitted input with output up to
5x larger stays under the documented 10,000-neuron free daily allowance even
at the most expensive allowed model's rates. Credential quota windows are
hard ceilings: a request whose estimate exceeds the remaining window is
rejected even when the window is empty, while a deployment quota still admits
one oversized request to an idle deployment.
Documentation warns that these are not account-wide neuron accounting or a
guarantee against paid overage.

#### Scenario: Retired Llama bridge target

WHEN an opted-in Cloudflare bridge leg targets `meta/llama-3.1-8b-instruct`
THEN it uses the reviewed current 3B `@cf/ibm-granite/granite-4.0-h-micro`
instead of sending the retired upstream ID. Documentation SHALL disclose the
checkpoint and size change. Other excluded deprecated models remain excluded.

#### Scenario: Optional Workers AI snapshot

WHEN `--workers-ai-models` or its API equivalent supplies a successful
Cloudflare model-search response
THEN every Workers AI deployment SHALL require its mapped upstream name to
appear in the snapshot as well as the static allowlist; absent names SHALL be
skipped with warnings. An empty result permits no Workers AI deployments.
Unreadable, malformed, or failed API responses SHALL abort import without
echoing their contents. Without a snapshot the static policy remains in effect.

### Requirement: Free-only keyed Kilo import

The importer SHALL keep keyed Kilo deployments keyed and SHALL require an
operator-supplied `--kilo-free-models` pricing snapshot with both prompt and
completion prices exactly zero. Missing, conflicting, or nonzero prices
SHALL fail closed. Anonymous Kilo behavior SHALL remain unchanged.

#### Scenario: External Kilo key

WHEN `--kilo-key-file` is supplied
THEN its contents are not opened or copied; only a `file:` credential is
emitted, and model eligibility still requires the pricing snapshot.

#### Scenario: Static pricing is not live enforcement

WHEN the operator supplies a snapshot
THEN documentation identifies the snapshot as static evidence rather than
live cost enforcement; paid models and missing pricing do not become keyed
deployments through names or free-status hints alone.

### Requirement: Direct DeepAI migration

The importer SHALL accept an API key for the built-in DeepAI connection and
route only `text-generator` and `summarization` to the official text API.
The importer SHALL retain a loopback `*/deepai/v1` sidecar as
`openai_compat` for a keyed `deepai-code` connection or OpenAI-compatible
node with prefix `deepai`. The DeepAI Code app serves its model proxy on a
random per-launch loopback port, so the allowlist covers any loopback host
with that path rather than one pinned port.
The regular loopback restrictions apply to other URLs. An explicit
same-model replacement option still takes precedence for bridge nodes.
The two native DeepAI text IDs take precedence over the Code node's
`deepai` prefix. The Code node's explicit ID remains distinct.

#### Scenario: Distinct protocols

WHEN an export contains keyed connections for the official DeepAI API and
the reviewed local DeepAI Code sidecar
THEN the importer selects `deepai` for supported text IDs
and `openai_compat` for the sidecar.

### Requirement: DeepAI Code discovery

With `--deepai-code-state PATH`, the importer SHALL load a discovery JSON
written by the review tooling: a loopback `base_url` for the app's model
proxy, a `free` set of catalog ids whose `theosPerRequest` is zero, and an
optional `scope` listing the models a per-harness token may serve. `free`
SHALL be an array, a present `scope` SHALL be an array, and malformed
discovery JSON or a non-loopback `base_url` SHALL fail the import. When
`--deepai-code-token-file PATH` is also given the importer SHALL emit a
`file:` credential for that token; the flag without `--deepai-code-state`
SHALL be an error. The discovered `base_url` SHALL replace the base URL of
every reviewed DeepAI Code connection — the built-in `deepai-code` identity
or an OpenAI-compatible custom node with prefix `deepai` — never an unrelated
keyed loopback bridge such as Cloudflare-direct (or attach to a new
lowest-priority connection when the export has none and a token was given).
For each imported combo model that maps to a `free`, in-scope catalog id the
importer SHALL append a free `openai_compat` leg against that connection,
which sorts ahead of billed legs with the other verified-free legs. When the
export already contains an equivalent deployment on that endpoint and model,
the importer SHALL reuse that deployment instead of emitting a duplicate that
would add selection weight or a duplicate-deployment warning. The same free
classification and session-token scope SHALL apply to existing exported legs
that use the reviewed DeepAI Code connection. Discovery-backed legs SHALL
also count when deciding whether a canonical combo is available for mapped
public aliases under `--openrouter-replacements`. Metered catalog ids, ids
outside the token scope, and catalog-free ids with no mapping SHALL emit no
leg; unmapped free ids SHALL be counted as warnings. Without
`--deepai-code-state` every deepai-code leg keeps its previous billed
treatment, including explicit `--free-providers` evidence for a custom
DeepAI node.

#### Scenario: Discovered free leg leads the canonical model

WHEN `--deepai-code-state` lists a loopback base URL and marks
`claude-haiku-5-5` free and the export's `deepai-code` connection is keyed
THEN the `claude-haiku-5-5` combo gains a free `openai_compat` leg at the
discovered base URL ahead of its billed legs.

#### Scenario: Session token attaches as a file credential

WHEN `--deepai-code-state` and `--deepai-code-token-file` are both given
THEN the deepai-code connection's credential is a `file:` reference to the
token path and the token value never appears in the config or warnings.

#### Scenario: Discovery without a keyed connection

WHEN `--deepai-code-state` and `--deepai-code-token-file` are given but the
export has no `deepai-code` connection
THEN a lowest-priority `deepai-code` connection is created at the discovered
base URL with the token credential.

### Requirement: Reviewed same-model bridge replacements

With `--openrouter-replacements`, the importer SHALL also replace only the
DuckDuckGo and DeepAI Code model IDs explicitly reviewed in `src/bridge.rs`
with the same declared model family, version and size on eligible connected
official endpoints. It SHALL NOT import bridge authentication, infer a similar
model, use Codex/Factory/OpenRouter targets, or relax Workers AI eligibility.

#### Scenario: Bridge leg expansion and precedence

WHEN a reviewed bridge leg appears in a combo, nested combo or direct alias
THEN its official deployments expand in place in the original leg order,
except that the five reviewed DeepSeek-flash combos place verified Morph ahead
of bridge replacements, the three fast combos start with Morph, and the two
base combos keep declared Numes ahead of Morph; source combo/alias/mapping
precedence is retained, including unavailable winning mappings.

#### Scenario: Source restrictions and missing targets

WHEN a bridge leg has valid source connection restrictions
THEN eligible official connections replace those restrictions with a warning,
as for Antigravity; unusable legs and malformed restrictions remain skipped.
WHEN its model is unreviewed or no eligible target is connected
THEN no replacement deployment is generated.

#### Scenario: Opt-in and explicit fallback

WHEN the flag is absent
THEN a keyed OpenAI-compatible bridge with a baseUrl is imported at that URL,
and DuckDuckGo stays dropped because it has no key-based HTTP API.
WHEN explicit OmniRoute fallback is configured
THEN it follows the normal last-deployment and Codex exclusion policy; bridge
replacement introduces no new fallback bypass.

### Requirement: Reviewed official OpenRouter replacements

With `--openrouter-replacements`, the importer SHALL route only reviewed
OpenRouter model IDs through eligible connected official endpoints or
opted-in Workers AI, using exact source-linked upstream IDs. It SHALL NOT
retain original OpenRouter or unverified mirror fallback legs on those names.
Unknown, unverified, and new-paid-key replacements SHALL be omitted with
warnings. Without the flag, existing OpenRouter routing SHALL be preserved.

#### Scenario: Replaced combo identity remains usable

WHEN an OpenRouter-named combo has an official replacement
THEN its name and retained ID remain resolvable by nested combo references,
`combo/` aliases, and model-combo mappings, using only the replacement.

#### Scenario: Unavailable winning mapping

WHEN the highest-precedence mapping targets a missing, inactive, or
deployment-empty combo
THEN its name and dependent aliases remain unavailable, even if a seeded
official replacement or lower-precedence mapping exists.

#### Scenario: Transitive mapping availability

WHEN clearing an unavailable winning mapping empties nested mapping targets
THEN replacement and combo availability is recomputed to a fixed point before
alias and mapping preparation, so dependent replacement names stay unavailable.

#### Scenario: Source connection restrictions

WHEN an OpenRouter leg has `connectionId` or nonempty `allowedConnectionIds`
THEN it is not rewritten onto an unrelated replacement connection.

#### Scenario: Explicit OmniRoute fallback cannot bypass replacement policy

WHEN `--omniroute-fallback` and `--openrouter-replacements` are both enabled
THEN Codex exclusions are computed from the original source combos before
rewriting, and OpenRouter namespaces, source aliases, mappings, and nested
routes receive no OmniRoute fallback deployment and are excluded from the
catch-all, whether their replacement was accepted or rejected.

#### Scenario: Compatibility and rollback

WHEN an operator needs the previous Cloudflare or OpenRouter policy
THEN removing their opt-in flags restores the previous replacement-only or
source-routing behavior. Keyed Kilo remains fail-closed without pricing
metadata; batch replacements are synchronous and preserve neither batch
scheduling nor batch discounts.

### Requirement: Collapse duplicate deployment legs

After a model's deployments are assembled, including per-deployment headers,
`strip_request_fields`, context and quota fields, and an optional OmniRoute
fallback leg, the importer SHALL drop a later leg that repeats an earlier leg
of that same model. Identity is the credential id, `upstream_model`,
`base_url`, per-deployment `headers`, `strip_request_fields`, `max_context`,
`quota`, and `timeout_ms`. Missing and empty header, strip, or quota values
are the same. A known `max_context` is not the same as a missing one, and two
different limits are not duplicates, so a request that fits only the larger
window still has a route. Header `file:` references are compared by the file
bytes when the file can be read at import time; an unreadable reference is
compared as the reference string. The kept leg is the one with the lowest
priority, then the earlier source position. The importer SHALL NOT collapse
legs across models.
It SHALL emit one warning per affected model naming how many legs were dropped,
and that warning SHALL NOT contain credential values.

Surviving non-fallback ids SHALL be renumbered to a contiguous
`<sanitized-model>-0` .. `<sanitized-model>-N` sequence, using the existing id
sanitizer so ids stay unique. Priorities SHALL be rewritten to `0..N` in that
kept order, except that a round-robin combo's equal native priorities stay
equal. An `omniroute-fallback-*` leg, including the catch-all, SHALL keep its
id and SHALL be dropped only when it is itself a duplicate of an earlier leg.
Its priority SHALL remain strictly greater than every surviving native leg of
that model, and contiguous with them. Models with no duplicate legs SHALL be
left unchanged.

#### Scenario: Repeated connection in one combo

WHEN one connection is listed in several steps of the same combo for the same upstream model
THEN that model has one deployment for the connection, its id and priority start at zero, and one warning names the model and how many legs were dropped.

#### Scenario: Same endpoint, different credentials

WHEN two connections share a base URL but use different credentials
THEN both deployments are kept, even if each connection is listed more than once.

#### Scenario: Headers or strip fields differ

WHEN two legs share a credential, upstream model, and base URL but differ in headers or `strip_request_fields`
THEN both are kept.

#### Scenario: Different context windows stay routable

WHEN two anonymous legs share an endpoint and upstream model but have context limits of 8192 and 32768
THEN both limits survive
AND a 20000-token request still has a deployment whose window fits it.

#### Scenario: Identical header files are one leg

WHEN two anonymous legs share an endpoint and their header files contain the same bytes
THEN one deployment remains.
WHEN a header file cannot be read at import time
THEN its reference string is the identity, so a different unreadable path is not collapsed.

#### Scenario: Fallback leg is not a native clone

WHEN a model has duplicate native legs and one OmniRoute fallback
THEN the fallback leg remains, with its `omniroute-fallback-*` id and a priority greater than every surviving native leg.

#### Scenario: Same leg in two models

WHEN two models each use the same connection and upstream model
THEN each model keeps its own deployment.

### Requirement: Dead and non-chat routes are never advertised

`/v1/models` SHALL list only models that can be served. The importer SHALL
accept `--exclude-models PATH`, a JSON array of model names (the shape
`llmux2-probe --dead-file` writes), and SHALL skip each listed name during
emit: its model entry, combo legs, and aliases resolving to it SHALL be
dropped with a warning, and with `--omniroute-fallback` the names SHALL join
the catch-all `exclude` set. Malformed or unreadable lists SHALL abort the
import before any output is written.

Upstream model IDs that cannot serve chat-completions-class endpoints —
embedding or typed-endpoint models listed in `src/policy.rs`'s reviewed
non-chat table — SHALL be classified out of every combo leg at emit, with a
warning; a combo left with no deployments SHALL be skipped as usual. The
match is on the emitted id's leaf segment with any `:variant` suffix
(`:free`, `:batch`) stripped, so gateway-routed ids such as
`openrouter/respan/span-01-lite` classify out with `span-01-lite`.

#### Scenario: Dead route dropped at import

WHEN a probe-confirmed dead model is listed in `--exclude-models`
THEN its name and aliases never reach `/v1/models`, and the catch-all cannot
serve it under another name.

#### Scenario: Embedding model classified out

WHEN a combo leg names a reviewed non-chat upstream model
THEN that leg is not emitted; a combo with only non-chat legs is not
advertised at all.
WHEN a combo leg routes a reviewed non-chat model through a gateway, such
that the emitted id keeps a routing path (for example
`openrouter/respan/span-01-lite` or its `:free` variant)
THEN that leg is not emitted either; unrelated routed models keep their legs.
