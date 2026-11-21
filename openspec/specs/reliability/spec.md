# reliability

## Purpose

Failure handling: failover, cooldowns, context limits, availability, and
deployment behavior under load.

## Requirements

### Requirement: Failover on limit and failure

The gateway SHALL detect 429, quota exhaustion, and upstream failures and
retry the request on the next eligible deployment.

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

### Requirement: Stream commitment

Once a streaming response has been committed, the gateway
SHALL NOT retry that request on another deployment.

#### Scenario: Mid-stream failure surfaces

WHEN the upstream fails after content events were already forwarded
THEN the error is surfaced to the client instead of restarting elsewhere.

### Requirement: Cooldowns

The gateway SHALL cool down a deployment after rate-limit or quota errors —
including ones that arrive after a stream has started — and skip it while it
cools down.

#### Scenario: Post-stream 429 cools down

WHEN a stream ends with a rate-limit error
THEN the deployment is skipped for new requests until its cooldown ends.

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
cooldown cap.

### Requirement: Rate-limit avoidance

The gateway SHALL reduce 429s proactively: secure additional paths, pace
requests, and pre-empt deployments before their limits hit, rather than only
reacting to errors.

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
