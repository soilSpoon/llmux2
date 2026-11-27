# reliability

## Purpose

Failure handling: failover, cooldowns, context limits, availability, and
deployment behavior under load.

## Requirements

### Requirement: Failover on limit and failure

The gateway SHALL detect 429, quota exhaustion, and upstream failures and
retry the request on the next eligible deployment. When the inbound body is
disk-spooled, each new deployment candidate SHALL materialize a fresh replay
copy from immutable request-side storage; a candidate's copy SHALL NOT be
retained for the rest of the request or used after a response commits.

#### Scenario: 429 triggers failover

WHEN a deployment answers 429
THEN the request is retried on the next eligible deployment without surfacing
the error to the client.

#### Scenario: An explicitly missing upstream model or an Anthropic HTTP 408 triggers failover

WHEN a deployment returns HTTP 400 or 404 with the explicit upstream error
code `model_not_found`, an OpenAI-compatible non-streaming success status
contains that explicit error instead of a completion, or the Anthropic adapter
receives HTTP 408
THEN the gateway SHALL try the next eligible deployment within the existing
attempt and time budgets, before committing a response. An ordinary upstream
400 or generic 404 SHALL remain terminal.

### Requirement: Same-deployment repair of a deprecated parameter

The gateway SHALL let a provider adapter repair a rejected request body and
resend it to the same deployment before any response is committed, at most three
times per attempt. The Anthropic adapter uses this to drop a `temperature`,
`top_p` or `top_k` that the upstream reports as deprecated for the model.

#### Scenario: Repair stays on the deployment

WHEN a repaired request is sent
THEN it targets the same deployment and counts as one HTTP attempt toward routing limits.

### Requirement: Stream commitment

The gateway SHALL commit a streaming response only on a frame that carries
content, a tool call, a finish reason, or usage, or on `[DONE]`. An
OpenAI-compatible keepalive (`choices` empty, and none of those fields) SHALL
NOT commit. Keepalives before that frame are dropped; keepalives after it are
forwarded. The wait for a committing frame is bounded by the existing request
and deployment timeouts.

An OpenAI-compatible assistant-role preamble without content, tool calls,
finish reason, usage or other output SHALL NOT commit. The first such preamble
is held until commitment and then sent before the committing frame. If the
attempt fails before commitment, its preamble is discarded.

Once a streaming response has been committed, the gateway
SHALL NOT retry that request on another deployment.

#### Scenario: Keepalives then content

WHEN an OpenAI-compatible stream sends keepalive chunks and then a content chunk
THEN the client receives the content chunk and any later keepalive, and does not
receive the keepalives that preceded the first committing frame.

#### Scenario: Mid-stream failure surfaces

WHEN the upstream fails after content events were already forwarded
THEN the error is surfaced to the client instead of restarting elsewhere.

#### Scenario: An error frame before commit fails over

WHEN an OpenAI-compatible upstream answers HTTP 200 `text/event-stream` and,
before any committing frame, sends an SSE data frame whose JSON has an `error`
object and no non-empty `choices`
THEN the gateway classifies that frame the same way as a non-streaming embedded
error (a 429 payload is rate limited, including a `Retry-After` hint from the
response headers; a quota marker is quota exhausted; an explicit client status
stays terminal; otherwise it is a server error) and tries the next eligible
deployment within the attempt budget. The failing deployment receives the same
cooldown as a non-stream error of that class. If every deployment fails that
way, the client receives the classified error, not an empty 200 stream.

### Requirement: Cooldowns

The gateway SHALL cool down a deployment after rate-limit or quota errors —
including ones that arrive after a stream has started — and skip it while it
cools down. Rate-limit hints and local backoffs stay capped by
`cooldown.max_ms`. A provider-reported quota reset is authoritative instead:
it SHALL be honored up to `cooldown.quota_max_ms` (default 32 days, always at
least `max_ms`), so a real reset horizon is not silently shortened to the
backoff cap. An unhinted quota exhaustion keeps the estimate cap, and a reset
reported by rate-limit headers uses the same quota cap.

#### Scenario: A Cloudflare daily quota cools until UTC midnight

WHEN a 429 body carries Cloudflare Workers AI's daily-quota signature — an
`errors[]` entry with code 4006 or 3036, or the "daily free allocation"
phrasing —
THEN the gateway treats it as credential quota exhaustion with an explicit
reset at the next 00:00 UTC, not as a transient rate limit. Other `errors[]`
codes (for example a capacity 429) keep the rate-limit path.

#### Scenario: A monthly quota resets on the first of the next month

WHEN a quota-exhausted error says the monthly quota is exceeded
THEN the credential cools until the first day of the next month at 00:00 UTC,
bounded only by `quota_max_ms`.

#### Scenario: A reset beyond the quota cap is clamped there

WHEN a provider reports a quota reset farther out than `cooldown.quota_max_ms`
THEN the cooldown ends at `quota_max_ms` rather than `max_ms` or the raw reset.

#### Scenario: Post-stream 429 cools down

WHEN a stream ends with a rate-limit error
THEN the deployment is skipped for new requests until its cooldown ends.

#### Scenario: A committed stream reports credential quota exhaustion

WHEN a committed stream ends with credential quota exhaustion
THEN the gateway surfaces the stream error without replay and applies the
same credential-wide cooldown as for a pre-commit quota error.

#### Scenario: Gemini reports an explicit quota violation

WHEN a Gemini 429 contains a daily `google.rpc.QuotaFailure` violation
THEN the gateway treats it as credential quota exhaustion rather than
deployment throttling. A positive `google.rpc.RetryInfo.retryDelay` takes
precedence over generic retry headers. Invalid or zero delays fall back to
other hints or local backoff.

#### Scenario: Anthropic exposes an exhausted rate-limit window

WHEN Anthropic reports zero remaining requests, total tokens, input tokens,
or output tokens with a future RFC3339 reset
THEN the gateway proactively skips that deployment until the applicable
reset. On a 429, a positive `Retry-After` takes precedence; otherwise the
latest future reset for an exhausted or unreported window is used. Invalid
or stale resets are ignored, and resets are subject to the configured
quota-reset cap (`cooldown.quota_max_ms`).

#### Scenario: Anthropic rejects an account with insufficient credit

WHEN Anthropic returns an HTTP 400 `invalid_request_error` beginning with
`Your credit balance is too low to access the Anthropic API.`
THEN the gateway treats it as credential quota exhaustion without an
upstream reset hint. It applies the existing cooldown to deployments sharing
the credential and tries the next eligible deployment if quota fallback is
enabled. Other HTTP 400s retain their existing behavior.

### Requirement: Unprovisioned deployments park until reload

When the provider reports that the credential's plan does not provision API
access for a deployment (for example a "no API allowance" error or a
`no_api_allowance` code), the gateway SHALL park that deployment until the
config reloads. Retrying it can never succeed, so it gets no cooldown and is
never re-probed; the credential itself is not disabled, and sibling
deployments on the same credential — including ones sharing its cooldown
bucket on a different route — stay eligible. The parked state SHALL be a
distinct failure class and skip reason in deployment stats and request logs
(`not_provisioned`), not a transient candidate failure, and SHALL count as
disabled in ops alerting.

#### Scenario: Missing allowance parks only that leg

WHEN a deployment answers that the plan has no API allowance
THEN the gateway marks it not-provisioned, later requests skip it without
consuming an attempt, and the credential's other deployments keep serving.

#### Scenario: Credential-scope cooling is distinguishable

WHEN an operator reads deployment stats during a credential-shared quota
cooldown
THEN each cooled deployment reports `cooldown_scope: "credential"`,
distinguishing the shared reset from a per-deployment backoff, and a parked
deployment reports `not_provisioned: true`.

### Requirement: Rate-limit avoidance

The gateway SHALL reduce 429s proactively: secure additional paths, pace
requests, and pre-empt deployments before their limits hit, rather than only
reacting to errors.

#### Scenario: Client retry timing on a synthesized 429

WHEN the gateway cannot route a request and synthesizes a 429
THEN it reports the earliest compatible deployment's known wait as a
rounded-up `Retry-After` in seconds. Zero means a compatible deployment is
ready now; when no wait can be determined the header is omitted. The gateway
does not forward raw upstream account limit headers.

#### Scenario: Pre-empt before the cap

WHEN a deployment's remaining limit approaches zero
THEN new requests are routed elsewhere before a 429 occurs.

### Requirement: Context-length rejection

On context overflow the gateway SHALL NOT fall back to a different or
larger-context model; it SHALL return a clear OpenAI-shaped 400
`context_length_exceeded` before calling upstream, using a pre-send token
estimate against the deployment's context size.

#### Scenario: Native Anthropic attachments

WHEN a native Messages request contains a base64 image or document, including in
tool result content
THEN the context estimate excludes the encoded attachment data but counts
surrounding text.

#### Scenario: Opt-in truncation

WHEN the caller opts into middle-out truncation
THEN the gateway keeps the system message and latest turns, drops middle
content, and reports the dropped count in a response header. Rejection stays
the default.

### Requirement: Always available

The gateway SHALL be designed to not go down: patches deploy with zero
downtime and no single failure takes the service offline.

#### Scenario: Zero-downtime patch

WHEN a new build is deployed
THEN in-flight requests finish and new requests keep being served without an
outage window.

### Requirement: Stability under load

The gateway SHALL stay stable under high request volume with minimal
overhead per request.

#### Scenario: Sustained high RPS

WHEN request volume stays high for an extended period
THEN latency and error rates stay within target without resource exhaustion.

### Requirement: Explicit request-path state machine

The attempt loop SHALL be driven by the pure transition function
`machine::State::step`, with phases `Select`, `Admit`, `Build`, `Gate`, `Send`,
`Refresh`, and `Done`. The following invariants SHALL hold and are model-checked
in `tla/` (names in parentheses):

- A retry never starts after a response was committed (`NoReplayAfterCommit`,
  `NoRetryAfterCommit`).
- Attempts never exceed `routing.max_attempts` and no attempt starts after the
  request deadline (`AttemptsBounded`, `NoAttemptAfterDeadline`).
- A permit is released on every terminal path: failure, exhaustion, stream end,
  idle timeout, mid-stream error, and client disconnect (`PermitReleasedOnExhaust`,
  `PermitReleasedWhenClosed`).
- A deployment in cooldown is not selected before `cooldown_until`
  (`NoSelectInCooldown`).
- During a slot handoff at least one slot accepts connections and a draining
  slot accepts none (`AlwaysOneAccepting`, `DrainingNotAccepting`).

#### Scenario: Transition table

WHEN `max_attempts` is reached in `Select` or `Admit`
THEN `step` returns `StopGroups` and no further attempt starts.
