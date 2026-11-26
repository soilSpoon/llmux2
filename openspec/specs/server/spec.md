# server

## Purpose

Inbound request policy that runs before any upstream call: end-user identity,
per-key model restrictions, request size, and image intake.

## Requirements

### Requirement: Inbound request body limit

`[server]` SHALL accept optional integer `max_request_bytes` in bytes,
defaulting to 134217728 (128 MiB). The value SHALL be positive; zero or a
negative value SHALL make config load fail. Every
inbound chat completions, Messages, or Responses request body larger than the
configured limit SHALL be rejected 413 with OpenAI-style code
`invalid_request_error` and message `Request body too large` before any
upstream call or Devin catalog refresh. A body exactly at the limit SHALL be
accepted for normal validation and routing. The inbound byte budget SHALL NOT
redact the gateway's own error response. Translating failures into Anthropic
Messages form SHALL use a fixed 16777216-byte internal error-payload budget
independent of `max_request_bytes`.

#### Scenario: Configured limit enforced before upstream

WHEN `max_request_bytes = 192` and a chat request body is 193 bytes
THEN the gateway returns 413 `invalid_request_error` and no upstream call is
made.

#### Scenario: Small positive limit preserves the Messages 413 contract

WHEN `max_request_bytes` is positive but smaller than the serialized 413 error
payload and a Messages request body exceeds it
THEN the gateway returns 413 with message `Request body too large` and
OpenAI-style code `invalid_request_error`.

#### Scenario: Oversized body does not start a Devin catalog refresh

WHEN the config contains a `devin_acp` deployment and a request body exceeds
the configured limit
THEN the gateway returns 413 without starting the `devin models` child
process.

#### Scenario: Boundary inclusive

WHEN a request body is exactly the configured limit
THEN the gateway accepts it for normal validation and routing.

#### Scenario: Multi-MiB default

WHEN the setting is absent and a valid chat request body is 3 MiB
THEN the gateway routes it.

#### Scenario: Nonpositive value rejected

WHEN `max_request_bytes` is zero or negative
THEN config load fails.

### Requirement: Request body disk spool

`[server]` SHALL accept optional `request_body_memory_bytes`,
`request_body_spool_dir`, and `request_body_spool_budget_bytes` settings.
`request_body_memory_bytes` SHALL be a positive byte count defaulting to
1048576 (1 MiB). `request_body_spool_budget_bytes` SHALL be a positive byte
count defaulting to 536870912 (512 MiB). `request_body_spool_dir` SHALL be a
nonempty path defaulting to `/var/tmp/llmux2-request-bodies` on Unix. A zero,
negative, or platform-overflowing byte count and an empty directory path SHALL
make config load fail.

Inbound request bodies whose buffered size stays at or below
`request_body_memory_bytes` SHALL remain memory-only. Once buffering another
chunk would cross that threshold, the gateway SHALL write the body incrementally
to an incomplete spool file instead of continuing to accumulate it in memory.
A completed spool SHALL be flushed and atomically renamed before use, SHALL have
0600 permissions on Unix, and SHALL be immutable thereafter. Spooling is
request-side only and SHALL NOT change streaming-response behavior.

`request_body_spool_budget_bytes` SHALL bound the total bytes reserved by all
in-flight request body spools in the process. Bytes SHALL be reserved before
they are written. If admission cannot reserve enough bytes, the gateway SHALL
fail closed with 503 before any upstream call, using a generic error that does
not copy request content, and SHALL remove every partial file for that request.
A successfully admitted spool SHALL be deleted when the request completes,
fails, or is cancelled, and its budget bytes SHALL be released. Startup SHALL
sweep llmux-named spool orphans without deleting files locked by a live
overlapping process.

When a spooled request fails over, the next upstream candidate SHALL be built
from a fresh read of the immutable spool rather than requiring the ingress body
to remain in memory. Gateway-only routing fields SHALL be stripped before that
canonical replay body is stored. A middle-out truncated retry of a spooled
request SHALL receive its own spool and lifecycle.

#### Scenario: Memory below threshold

WHEN a request body is smaller than `request_body_memory_bytes`
THEN no request-body spool file is created and routing behaves as before.

#### Scenario: Failover replays a spooled body

WHEN a body crosses the memory threshold and the first deployment returns a
retryable 429
THEN the next deployment receives the same upstream body and the spool file is
removed after the response.

#### Scenario: Budget exhaustion fails closed

WHEN live spool reservations leave insufficient budget for a new large body
THEN the new request receives 503, no upstream call is made, no partial spool
remains, and response text does not contain request-body content.

#### Scenario: Cancellation and errors clean up

WHEN a spooled request is cancelled or routing ends in an error
THEN its temporary or final spool file is removed and its disk budget is
released.

#### Scenario: Startup sweep preserves overlap

WHEN the gateway starts in a spool directory containing an unlocked orphan
from a crashed process and a locked file owned by a live overlapping process
THEN it removes the orphan and leaves the live file intact.

### Requirement: Per-key model policy

`[server]` SHALL accept optional `[[server.key_policies]]` entries, each naming
one `server.client_keys` secret reference as `key` plus glob lists `models` and
`blocked`, an optional `free_only` flag, and an optional `traffic_class`.
A client key with no entry SHALL be unrestricted and unclassified. A `free_only = true` entry SHALL force free-leg-only routing for
that key's requests and SHALL filter `GET /v1/models` to models with a usable
free leg (see the routing spec's adaptive free-leg advertising). For a listed key,
the requested `model` field — before alias, canonical, or catch-all resolution —
SHALL match at least one `models` glob (an absent or empty `models` allows every
name) and no `blocked` glob, on every inbound format that resolves a model;
otherwise the gateway SHALL return 403 `model_not_allowed` before any upstream
call, in the same error shape as per-model `allowed_keys`. Globs SHALL match
ASCII case-sensitively with `*` spanning any substring including `/` and `?`
matching exactly one character (a Unicode scalar value, not one byte).

`traffic_class` SHALL be one of `ci_buyer_shaped`, `synthetic_probe`,
`internal`, or `unknown`. When present, the gateway SHALL attach that class to
request accounting solely because the request authenticated with the matching
client key. Caller-supplied headers or body fields SHALL NOT create or change
the class. A key whose policy has no `traffic_class` SHALL be accounted as
`unknown`.

Config SHALL reject a `key_policies` entry whose `key` does not resolve to a
`server.client_keys` value, a repeated key, an empty pattern, and an entry with
no `models`, no `blocked`, no `free_only`, and no `traffic_class` (allow-all
minus nothing is meaningless). The
check SHALL use the same matched-key identity as authentication and
`allowed_keys`, so it covers chat completions, `/v1/responses`, and Anthropic
`/v1/messages` keys presented as bearer or `x-api-key` credentials.

`GET /v1/models` SHALL list only names the requesting key's policy allows; an
unrestricted key's list SHALL be unchanged and the `*` catch-all SHALL never
appear for either.

#### Scenario: Listed key denied a non-matching name

WHEN a restricted key requests a model no `models` glob matches
THEN the gateway returns 403 `model_not_allowed` without an upstream call.

#### Scenario: Blocked wins over allowed

WHEN a requested name matches both `models` and `blocked`
THEN the request is rejected 403 even though an allow glob matched.

#### Scenario: Unlisted key unchanged

WHEN a key has no `key_policies` entry
THEN any configured or catch-all-routed name works as before
AND `GET /v1/models` returns the full configured list.

#### Scenario: Listing filtered

WHEN a restricted key calls `GET /v1/models`
THEN only configured names and aliases matching its policy appear
AND `*` is not listed for any key.

#### Scenario: Policy bound to a non-client key

WHEN `key_policies[].key` resolves to a secret absent from `client_keys`
THEN config load fails with a `client_keys` validation error.

#### Scenario: Probe identity cannot be forged

WHEN a request authenticated with a `ci_buyer_shaped` key sends a caller-asserted probe header
THEN request accounting remains `ci_buyer_shaped`; only an authenticated key whose policy names `synthetic_probe` receives that class.

### Requirement: Global model deny

`[server]` SHALL accept an optional `deny` list of glob patterns. The
requested `model` field — before alias, canonical, or catch-all resolution —
SHALL match no `deny` glob on every inbound format that resolves a model,
for every key including ones without a `key_policies` entry; otherwise the
gateway SHALL return 403 `model_not_allowed` before any upstream call, in the
same error shape as per-model `allowed_keys`. Deny globs use the same
matching semantics as `key_policies` patterns. Config SHALL reject an empty
`deny` pattern.

A denied name cannot be requested by any key, so `GET /v1/models` SHALL hide
denied names for every caller. This is the guard against auto-resolve
bypass: a `provider/model` id that no configured model names would otherwise
ride the `*` catch-all to a billed upstream; listing it in `deny` closes that
route without touching the catch-all itself.

#### Scenario: Denied name rejected for every key

WHEN any key requests a model matching a `deny` glob, including an
unrestricted key with no `key_policies` entry
THEN the gateway returns 403 `model_not_allowed` without an upstream call.

#### Scenario: Denied unknown name does not reach the catch-all

WHEN the requested name is not configured, matches a `deny` glob, and would
otherwise resolve through the `*` catch-all
THEN the request is rejected 403 and no upstream call is made.

#### Scenario: Listing hides denied names

WHEN any key calls `GET /v1/models`
THEN configured names matching a `deny` glob do not appear.

#### Scenario: Empty pattern rejected

WHEN a `deny` entry is an empty string
THEN config load fails with a validation error.

### Requirement: Hashed end-user identity

The gateway SHALL leave end-user attribution off unless `[server]
end_user_header` is set. When it is set, `end_user_salt` SHALL be required and
SHALL be a nonempty `env:` or `file:` secret; a literal, a missing secret, or
an empty resolved value SHALL be rejected without echoing the value. The
header name SHALL be a valid HTTP header name and SHALL NOT be a credential,
cookie, or hop-by-hop header. `end_user_from_body` SHALL default to true when
the header is set and SHALL be rejected without the header.

When the configured header is present exactly once and its value is non-empty
text, the gateway SHALL set the end-user id to lowercase hex SHA-256 of the
UTF-8 salt bytes immediately followed by the UTF-8 header value, with no
separator. A missing, empty, or whitespace-only header SHALL fall through to
the OpenAI top-level string `user` field when `end_user_from_body` is true,
hashed the same way. Duplicate or non-text header values SHALL be 400 and
SHALL NOT be logged or copied into the error. The raw identifier SHALL NOT be
logged, sent upstream, or returned. The response SHALL include
`x-llmux-end-user` with the hash only, including on later gateway errors,
independent of `expose_routing_headers`.

#### Scenario: Header beats the body field

WHEN the configured header and an OpenAI `user` field are both present
THEN the hash is of the header value, the response header is that hash, and
the raw values appear in neither the response nor the upstream body.

#### Scenario: Feature off

WHEN `end_user_header` is unset
THEN no end-user hash is derived and no `x-llmux-end-user` response header is
added.

### Requirement: Remote image policy

`remote_images` SHALL be `allow` (the default) or `reject`. With `reject`, an
`http` or `https` image URL in an inbound chat, Messages, or Responses body
(`image_url` string or object, or an Anthropic image `source.url`) SHALL be
answered 400 with the OpenAI-style message `remote image URLs are not
accepted; send a data: URL` before any upstream call. Optional
`max_image_bytes` SHALL be positive when set and SHALL reject a `data:` image
or a native Anthropic base64 image whose decoded payload exceeds the limit
with 413. The check SHALL live in one inbound validation step, not in each
adapter. `allow` with no limit SHALL preserve previous image handling.
Tool definitions and output-format schemas SHALL NOT be inspected as image
inputs; image blocks in message content, including nested tool results,
remain subject to the policy.

#### Scenario: Remote URL rejected

WHEN `remote_images = "reject"` and a chat message contains an https
`image_url`
THEN the client receives 400 and the upstream is not called.

#### Scenario: URL examples in a schema are permitted

WHEN a tool or output-format schema contains an `image_url` example, but
the request has no disallowed image input
THEN the image policy lets the request proceed without changing the schema.

#### Scenario: Inline image over the cap

WHEN `max_image_bytes` is set and a `data:` image decodes to more bytes
THEN the client receives 413 and the upstream is not called.

#### Scenario: Default allows remote URLs

WHEN the options are unset
THEN an https `image_url` is forwarded as before.

### Requirement: Per-key model policy

`[[server.key_policies]]` entries MAY restrict which model names a client key
can request. `key` SHALL name an entry of `server.client_keys`; `models` SHALL
be a nonempty glob list whose presence makes the key restricted, and `blocked`
SHALL be a glob list subtracted after `models` matches. A key with no policy
entry is unrestricted. Restricted keys SHALL get 403 `model_not_allowed` for a
non-matching or blocked name on every model-resolving endpoint, evaluated on
the requested name before alias resolution, and `/v1/models` SHALL list only
names the key may request.

#### Scenario: Restricted key allowed and denied

WHEN key `ci` has `models = ["claude-*"]` and `blocked = ["*-billed"]`
THEN `claude-sonnet-5.5` proceeds, `gpt-5` and `claude-opus-billed` return 403,
and `/v1/models` for `ci` shows only the surviving names.

#### Scenario: Unrestricted key unchanged

WHEN a key has no `key_policies` entry
THEN all names resolve exactly as before, including the catch-all.
