# server

## Purpose

Inbound request policy that runs before any upstream call: end-user identity,
per-key model restrictions, and image intake.

## Requirements

### Requirement: Per-key model policy

`[server]` SHALL accept optional `[[server.key_policies]]` entries, each naming
one `server.client_keys` secret reference as `key` plus glob lists `models` and
`blocked` and an optional `free_only` flag. A client key with no entry SHALL be
unrestricted. A `free_only = true` entry SHALL force free-leg-only routing for
that key's requests and SHALL filter `GET /v1/models` to models with a usable
free leg (see the routing spec's adaptive free-leg advertising). For a listed key,
the requested `model` field — before alias, canonical, or catch-all resolution —
SHALL match at least one `models` glob (an absent or empty `models` allows every
name) and no `blocked` glob, on every inbound format that resolves a model;
otherwise the gateway SHALL return 403 `model_not_allowed` before any upstream
call, in the same error shape as per-model `allowed_keys`. Globs SHALL match
ASCII case-sensitively with `*` spanning any substring including `/` and `?`
matching exactly one character (a Unicode scalar value, not one byte).

Config SHALL reject a `key_policies` entry whose `key` does not resolve to a
`server.client_keys` value, a repeated key, an empty pattern, and an entry with
no `models`, no `blocked`, and no `free_only` (allow-all minus nothing is
meaningless). The
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
