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

### Requirement: Dahl account-key rotation

A `dahl` credential SHALL authenticate inference with the current account API
key from its local state file and SHALL refresh that key when upstream rejects
it as invalid. Refresh SHALL sign in with the configured fingerprint, inspect
the account pool and owned key allocation, create a replacement key, allocate
an explicit token budget, persist the replacement state atomically before use,
and only then make the replacement current. Rotation SHALL NOT create quota;
it moves tokens from Dahl's shared account pool. The operator SHALL choose the
budget with `allocation_tokens` or explicitly opt into `allocate_entire_pool`.

The state file SHALL distinguish keys created by llmux (`owned`) from imported
or user-managed keys. Rotation SHALL revoke only an owned previous key, because
Dahl returns its remaining allocation to the shared pool. It SHALL leave
user-managed keys untouched. Fingerprints, API keys, and session cookies SHALL
NOT appear in errors or logs.

#### Scenario: Invalid Dahl key rotates and retries

WHEN Dahl rejects the current key as invalid
THEN the gateway signs in with the fingerprint, creates and allocates a
replacement key, atomically persists it, revokes only the owned previous key,
and retries the same deployment with the new bearer token.

#### Scenario: Empty account pool

WHEN the Dahl account pool and the owned previous key have no tokens available
THEN refresh fails without switching credentials or treating rotation as new
quota.

### Requirement: Free and usage-limited providers

The gateway SHALL support providers that are free or carry usage limits
(rate limits, periodic quota, expiring quota) alongside paid ones.

#### Scenario: Free-tier provider

WHEN a provider only offers a free tier with a request cap
THEN it is configured as a normal deployment and its limits are tracked like
any other.

### Requirement: Free-only use of mixed providers

A provider that offers free and paid models on one credit balance SHALL serve
the gateway only with its free models. Paid-model legs on such a provider
SHALL be marked billed and SHALL NOT be emitted or enabled without explicit
owner opt-in; a credit balance SHALL NOT by itself make a paid leg eligible.
Each provider's free-model conditions — plan tier, free model list, `:free`
suffixes, quota classes — SHALL be recorded so billing classification stays
accurate per model, not per provider.

#### Scenario: Credit does not unlock paid legs

WHEN a mixed free/paid provider account holds paid credit
THEN only its free models carry deployments, and paid models are never served
on that credit — even when every free leg is exhausted.

### Requirement: DeepAI text API

The gateway SHALL convert non-streaming Chat Completions requests with one
user text message for keyed `text-generator` and `summarization` deployments.
It SHALL reject unsupported options and send the key in
`Api-Key`, not `Authorization`. DeepAI Code SHALL use the existing
`openai_compat` adapter when its gateway speaks Chat Completions.

#### Scenario: DeepAI Code lane

WHEN a deployment uses the `deepai_code` provider kind
THEN the gateway SHALL send the credential as `Api-Key`, never as
Authorization, and SHALL forward the deployment's configured
`x-deepai-code-theos` header unchanged, because the lane refuses a request
without it.
AND a `403` whose code or type is `theos_exhausted` or `theos_insufficient`
SHALL be classified as a spent quota rather than an unusable credential, while
`social_login_required` and `machine_required` SHALL remain credential
refusals.

#### Scenario: DeepAI text response

WHEN a configured `deepai` deployment receives a text chat request
and DeepAI returns a string `output`
THEN the gateway returns the output as assistant text and identifies the
configured upstream model in the response.

### Requirement: Freebuff via Codebuff Chat Completions

A deployment with a `freebuff` API key credential SHALL use the Codebuff
OpenAI-compatible Chat Completions endpoint. The adapter SHALL append
`/chat/completions` to the configured base URL (Codebuff production:
`https://www.codebuff.com/api/v1`), authenticate with an Authorization bearer
key, and send the configured upstream model ID as supplied by Codebuff (for
example, `deepseek/deepseek-v4-flash`). The adapter SHALL NOT emulate the
upstream's Freebuff session admission or grant free-tier eligibility.

#### Scenario: Configured Freebuff deployment

WHEN a deployment references a `freebuff` credential and a configured upstream
model ID
THEN the gateway initializes and forwards Chat Completions JSON and SSE, and
translates OpenAI Responses and Anthropic Messages via Chat Completions.

#### Scenario: Freebuff rate limit

WHEN Codebuff returns HTTP 429 with a Retry-After header
THEN the gateway classifies it as rate limited and retains the retry delay.
### Requirement: AWS Quick through Magpie

An `aws_quick` deployment SHALL send Chat Completions to the Magpie gateway.
It SHALL accept `auth = "none"` and default to `http://127.0.0.1:3425` when
`base_url` is omitted. It SHALL convert OpenAI Responses and Anthropic Messages
requests to Chat Completions and return responses in the caller's format.

The adapter SHALL set both `model` and `agent_mode` to the mode selected by
`upstream_model`, or by the requested model for a catch-all deployment.
It SHALL recognize `fast`, `balanced`, `smart`, and `advanced` as bare modes
or after a slash-separated prefix. It SHALL map
`us.anthropic.claude-haiku-5-5` to `fast` and
`us.anthropic.claude-opus-5-5` to `smart`. Unrecognized model names SHALL
select `smart`.

The adapter SHALL translate recognized client effort to Magpie's
`off`, `low`, `medium`, `high`, or `max`, send it in `thinking_effort`,
and set `x-magpie-effort` unless the deployment already sets that header.
Unrecognized effort SHALL NOT add a header or a body effort value.

#### Scenario: Responses request selects fast mode and maximum effort

WHEN an `aws_quick` deployment with `upstream_model = "aws-quick/fast"`
receives an OpenAI Responses request with `reasoning.effort = "xhigh"`
THEN the upstream Chat Completions request sends `model` and `agent_mode`
as `fast` and sends `max` in both `thinking_effort` and `x-magpie-effort`.
The gateway returns the upstream result in OpenAI Responses format.

#### Scenario: Unknown model falls back and the configured header wins

WHEN an `aws_quick` deployment has an unknown upstream model and sets
`x-magpie-effort: low`, and a Chat Completions request asks for
`reasoning_effort = "high"`
THEN the upstream body selects `smart` and contains `thinking_effort: high`,
but the upstream header remains `x-magpie-effort: low`.

### Requirement: Devin ACP effort selects the model tier

The `devin_acp` provider SHALL translate a recognized `reasoning_effort`,
`thinking_effort`, or `effort` value in the request body to a Devin catalog
tier and rewrite the deployment `upstream_model`'s published tier suffix
(`-medium`, `-high`, or `-max`, preserving a trailing `-lightning`). Recognized
spellings map only onto tiers Devin lists free: `off`, `none`, `disabled`,
`false`, `low`, `minimal`, `medium`, `med` select `medium`; `high`, `enabled`,
`true` select `high`; `max`, `xhigh` select `max`. An absent or unrecognized
effort SHALL keep the deployment's configured `upstream_model` unchanged, and a
model id without a tier suffix SHALL NOT gain one from effort alone.

#### Scenario: Effort selects the tier

WHEN a request carries `"reasoning_effort": "max"` against a deployment whose
`upstream_model` is `swe-2-high`
THEN the ACP session opens with model `swe-2-max`
AND the response reports `swe-2-max`.

#### Scenario: Free tiers only

WHEN a request carries `"reasoning_effort": "low"` against a deployment whose
`upstream_model` is `swe-2-flash-high`
THEN the ACP session opens with `swe-2-flash-medium`, the lightest published
free tier, and not a paid or unpublished id.

#### Scenario: No effort or unknown spelling

WHEN the request carries no effort field or an unrecognized value
THEN the deployment's configured `upstream_model` is used unchanged.

### Requirement: Devin ACP replies and emulated tool calls

The `devin_acp` provider SHALL drive the default `devin acp` agent — not a
task-specific agent type such as `summarizer`, which emits conversation
summaries on long transcripts — and frame the flattened transcript so the
reply is a single assistant message. Each session SHALL be placed in `ask`
mode via `session/set_mode` so the agent's own tools are disabled agent-side;
a `session/request_permission` that arrives anyway SHALL be answered with the
`cancelled` outcome rather than a JSON-RPC error, and unrelated client-capability
methods MAY still be answered `method not supported`. When the request carries
a non-empty `tools` array and `tool_choice` is not `"none"`, the prompt SHALL
present the tool schemas and a structured-call protocol; `"required"` demands
at least one call, a `{"type":"function","function":{"name":…}}` choice demands
that named tool, and `parallel_tool_calls: false` caps the protocol at one
call. A reply consisting of a `{"tool_calls":[…]}` object SHALL be emitted as
OpenAI `tool_calls` with `finish_reason: "tool_calls"`, with `id`, `type`, the
`function` wrapper for flat `{"name","arguments"}` calls, and string-form
`arguments` filled in when the model omits or object-encodes them. Any other
reply SHALL be emitted as text content. Tool turns never stream incrementally:
the reply is buffered and emitted in one shot because the JSON must be complete
before it can be classified, but the stream SHALL open with the role-only delta
once the first `session/update` arrives so a buffered turn is not silent.
Assistant `tool_calls` turns and `tool` results SHALL keep their call identity
in the transcript (`tool(<id>[, <name>]): …`). The `session/prompt` usage
block, when present, SHALL be mapped to OpenAI `usage` (`prompt_tokens`,
`completion_tokens`, `total_tokens`, `prompt_tokens_details.cached_tokens`);
streams emit it only when `stream_options.include_usage` is true.

#### Scenario: Tool call reply

WHEN a request offers a `read` function tool and the agent replies with
`{"tool_calls":[{"function":{"name":"read","arguments":{"path":"/etc/hostname"}}}]}`
THEN the completion carries `finish_reason: "tool_calls"`, null `content`, and
`tool_calls` with `id`, `type: "function"`, and `arguments` as a JSON string.

#### Scenario: Plain reply with tools offered

WHEN tools are offered but the agent replies with plain text
THEN the completion carries that text as content and `finish_reason: "stop"`.

#### Scenario: Tool choice none

WHEN the request offers tools but sets `"tool_choice": "none"`
THEN the prompt carries no tool schemas or tool-call protocol and the reply is
always emitted as text.

#### Scenario: Internal tools stay off

WHEN a request is served
THEN the session enters `ask` mode before `session/prompt`, so an agent tool
attempt is refused structurally rather than by prompt wording.

#### Scenario: Permission request during a turn

WHEN the agent issues `session/request_permission` mid-turn despite `ask` mode
THEN the bridge answers `{"outcome":{"outcome":"cancelled"}}` and the turn
still completes.

#### Scenario: Usage on a streamed turn

WHEN a stream request sets `stream_options.include_usage`
THEN a final `choices: []` chunk carries the mapped usage before `[DONE]`.

### Requirement: Devin ACP image forwarding

The `devin_acp` provider SHALL forward client-supplied image parts so the agent
can actually see them. ACP `image` content blocks are acknowledged at the
protocol level but never shown to the model (verified on devin 3000.14.30821:
the agent reports "no image was provided"), so inline image bytes SHALL be
decoded and written to files under `<home>/.llmux-images/<unique>/` for the
agent to `read` from its sandbox — the only channel that reaches the model.
OpenAI `image_url`/`input_image` parts (string or `{url}` object) and Anthropic
`image` source parts are detected in message order. The transcript SHALL mark
each image's position with `[image N]` and end with an `[Images]` index listing
`image N: <path-or-url>` in the same order, so the model can pair image and
surrounding turn. Remote references SHALL be indexed by URL only when they are
`http://` or `https://` — a bare path or `file://` URL is dropped, since the
index invites the agent to `read` it and would expose sandbox files; non-base64
`data:` URLs degrade the same way. Staged files SHALL be removed
when the turn ends, including aborted turns. A single image whose decoded
bytes exceed 8 MiB SHALL fail the request `413 Payload Too Large` — checked
by encoded length before decode so the payload is never materialized — and
malformed base64 SHALL fail `400`. Messages carrying only images SHALL still
produce a turn.

#### Scenario: Multimodal user message

WHEN a user message carries `[{"type":"text","text":"what is this?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]`
THEN the bytes are staged under `<home>/.llmux-images/`, the transcript text
contains `user: what is this?\n[image 1]`, and an `[Images]` index names the
staged file path so the agent reads it.

#### Scenario: Remote image URL

WHEN an image part references `https://example.test/x.png`
THEN the transcript marks the position and indexes the URL itself — no file is
staged — and the turn still completes.

#### Scenario: Local-path image reference

WHEN an image part references a non-HTTP(S) URL such as
`file:///etc/passwd` or `.local/share/devin/credentials.toml`
THEN the reference produces no `[image N]` index entry, so the agent is never
invited to read a sandbox-local file it was not given.

#### Scenario: Staging cleanup

WHEN a turn with staged images completes or is aborted
THEN the request's `.llmux-images/<unique>/` directory is removed, leaving no
payload files behind.

### Requirement: ACP agent CLIs as managed children

The `acp_agent` provider SHALL serve an OpenAI chat request by spawning the
agent CLI named by the deployment's `acp_agent` credential as a child process
and speaking ACP JSON-RPC on its stdio (`initialize`, `session/new`,
`session/set_mode` "ask", `session/prompt`). The credential SHALL declare the
child's `command` as an absolute path, plus `args`, an `env` allowlist, a
sandbox `home`, an optional `cwd` defaulting to `home`, an optional
`turn_timeout_ms`, an optional `session_mode`, and an optional `auth_method`.
The child SHALL run under a
scrubbed environment whose `HOME` and XDG directories point at `home` (so the
CLI resolves its own config and sign-ins there), with only the declared `env`
entries added on top; `env` values MAY use `env:`/`file:` references like
deployment headers. The credential SHALL map an API-key-less credential id to
the `acp_agent` provider kind, and a deployment referencing it SHALL default
its `base_url` to the placeholder endpoint.

`session_mode` SHALL default to `per_request`: a fresh child is spawned,
handshaken, spoken to, and killed inside `turn_timeout` for each request, and
the child SHALL also be killed when the attempt's deadline or cancellation
drops the turn. `session_mode = "persistent"` SHALL keep one initialized child
per credential id and multiplex `session/new` turns through it, serializing
concurrent requests; a dead child SHALL be dropped and lazily respawned, and a
turn that fails with a definitive RPC error response SHALL keep the child
while any other failure drops it.

`auth_method` SHALL name an ACP `authenticate` method id; when set, the
handshake SHALL call `authenticate` with it after `initialize` and before any
`session/new` — for an agent with cached credentials this is a silent token
refresh, while an agent that is not signed in SHALL fail or stall the spawn
(and thus the turn, bound by `turn_timeout`) rather than reach `session/new`
unauthenticated.

Reply chunks from `session/update` `agent_message_chunk` notifications SHALL
stream as OpenAI deltas and aggregate into the non-streamed completion;
`session/request_permission` SHALL be answered `cancelled` like the Devin
bridge, and a `session/prompt` usage block SHALL map to OpenAI `usage`. A
spawn failure SHALL surface as a connect-class error, the child exiting or
closing stdio mid-turn as a stream-broken error, the turn exceeding
`turn_timeout` as a timeout error, a `{`-led line that is not valid JSON-RPC
as a server error rather than a hang, and a well-formed RPC error response
SHALL map to its failover class (rate-limited, quota-exhausted, model-not-found,
or server). Non-JSON noise lines (banners) SHALL be skipped. Credential values
— including resolved `env` secrets — SHALL NOT appear in errors, logs, or
other output.

Agent CLIs speaking a different JSON-RPC dialect (Codex `app-server`'s
`thread/start`/`turn/start`, zcode) SHALL NOT be claimed by this provider;
they are a separate protocol family.

#### Scenario: Per-request spawn and cleanup

WHEN a chat request hits an `acp_agent` deployment in the default mode
THEN a fresh child runs the credential's command and args under the sandboxed
environment, the ACP handshake and prompt complete, the reply maps to an
OpenAI completion, and the child is no longer running afterwards.

#### Scenario: Child exits non-zero

WHEN the CLI exits before answering (for example on a missing sign-in)
THEN the request fails with a stream-broken or connect-class error and no
request hangs.

#### Scenario: Turn timeout

WHEN the child never answers within `turn_timeout`
THEN the request fails with a timeout error and the child is killed.

#### Scenario: Malformed JSON-RPC frame

WHEN the child emits a `{`-led line that is not valid JSON-RPC
THEN the request fails with a server error instead of waiting for a well-formed
response that cannot come.

#### Scenario: Persistent reuse

WHEN a credential sets `session_mode = "persistent"` and serves two requests
THEN one child is initialized once and both turns run through it; when the
child has died, the next request respawns it.

#### Scenario: Sandbox home

WHEN the credential declares `home` and `env` entries
THEN the child sees `HOME` and the XDG dirs inside `home`, plus exactly the
declared variables, with the parent's other environment scrubbed.

### Requirement: Antigravity v1internal streaming

An `antigravity` deployment SHALL send the upstream request to
`/v1internal:streamGenerateContent?alt=sse` with the deployment's
`x-llmux-project` header and a User-Agent identifying a current Antigravity
client release. The upstream gates on the advertised client version and
answers a stale version with a 200 carrying only an unsupported-version
notice, so the UA moves with the real client line.

The upstream stream ends at EOF without a `data: [DONE]` frame. The adapter
SHALL treat EOF on a clean SSE event boundary as end-of-stream and EOF with
bytes still pending in an incomplete event as stream-broken.

#### Scenario: DONE-less upstream stream completes

WHEN an antigravity upstream closes the response after a finish-reason event
with no `[DONE]` frame
THEN the turn completes normally with the streamed content.

#### Scenario: Mid-event EOF

WHEN the upstream EOFs with a partial SSE event buffered
THEN the attempt fails as stream-broken.

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

### Requirement: Anthropic sampling parameter compatibility

Current Claude models (Claude 5 generation, Fable 5, Opus 4.8) reject
`temperature`, `top_p` and `top_k` with HTTP 400 `` `<field>` is deprecated for
this model`` unless the value is the default. The Anthropic adapter SHALL accept
`strip_request_fields` limited to `temperature`, `top_p` and `top_k` and omit
them from the upstream body on both the OpenAI Chat conversion and the native
Messages passthrough. When an upstream still answers such a 400 for a field
that was sent, the adapter SHALL remove only that field and log which field was
dropped. The gateway SHALL resend to the same deployment only if its cooldown
and deployment and credential quotas allow another upstream request. If they
do not, it SHALL return the original 400. The retry SHALL NOT
move to another deployment, so the served model never changes, and SHALL NOT
apply to any other 400.

#### Scenario: Configured stripping

WHEN an anthropic deployment sets `strip_request_fields = ["temperature", "top_p"]`
THEN neither field reaches the upstream on /v1/chat/completions or /v1/messages,
and other fields are untouched.

#### Scenario: Deprecation rejection heals

WHEN the upstream answers 400 `` `temperature` is deprecated for this model``
and quota allows another upstream request
THEN the same deployment is retried once without `temperature`, and the client
receives that retry's response.

#### Scenario: Other bad request

WHEN the upstream answers any other 400
THEN it is returned to the client without a retry.

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

### Requirement: Multiple accounts per provider

The gateway SHALL support several credentials of one provider — multiple
accounts or plans — each as its own deployment with its own quota tracking.
Ordering between them follows the normal routing rules, since two accounts of
one provider can carry different plans; they are never pooled into one limit.

#### Scenario: Two accounts of one provider

WHEN two OAuth connections of the same provider are imported
THEN the model carries one deployment per connection, each tracked against
its own account quota window rather than a shared one.

### Requirement: Persistent credential disable

A credential MAY set `disabled = true`; the gateway SHALL treat it exactly
like an admin-API disable — its deployments never route — and the state
SHALL survive reloads because it is re-applied on every config load. The
admin credential surface SHALL report the live flag and whether the config
requests the disabled state; the admin API MAY still re-enable a
config-disabled credential until the next reload restores the flag.

#### Scenario: Disabled credential survives reload

WHEN a credential carries `disabled = true` and the config reloads
THEN the credential stays disabled and its deployments never serve traffic,
while the admin list reports the config-sourced state.

### Requirement: Excluded providers and practices

The gateway SHALL NOT integrate OpenRouter, Factory, Augment, Z.AI, or
multi-IP distribution to raise limits, and SHALL NOT
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

### Requirement: Hashed end-user attribution

When `[server] end_user_header` is set and the gateway has derived an end-user
hash, the Anthropic adapter SHALL set `metadata.user_id` to that hash on both
the OpenAI Chat conversion and the native Messages passthrough, overriding a
client-supplied `user` or `metadata.user_id` and preserving other metadata
fields on passthrough. An `openai` or `openai_compat` deployment SHALL set the
Chat Completions `user` field to the same hash, then apply `strip_request_fields`,
so a deployment that strips `user` does not send it. Other provider kinds SHALL
NOT gain a `user` field from this feature. When no hash is present, adapters
SHALL keep their previous behavior, including copying a client `user` into
Anthropic `metadata.user_id`.

#### Scenario: Hash overrides a client id

WHEN a request carries an end-user hash and a client `user` or `metadata.user_id`
THEN the Anthropic upstream receives the hash as `metadata.user_id` and an
OpenAI-compatible upstream receives it as `user`, and neither body contains the
raw identifier.

#### Scenario: Stripped user field

WHEN an `openai_compat` deployment lists `user` in `strip_request_fields`
THEN the hashed `user` field is omitted from the upstream body.

#### Scenario: No hash

WHEN the end-user feature is off or the request has no identifier
THEN Anthropic still copies a client `user` into `metadata.user_id` and does not
invent one.
