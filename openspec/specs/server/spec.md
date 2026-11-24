# server

## Purpose

Inbound request policy that runs before any upstream call: end-user identity
and image intake.

## Requirements

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
