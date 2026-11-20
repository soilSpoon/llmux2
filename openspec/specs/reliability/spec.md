# reliability

## Purpose

Failure handling: failover, cooldowns, context limits, availability, and
deployment behavior under load.

## Requirements

The gateway SHALL detect 429, quota exhaustion, and upstream failures and
retry the request on the next eligible deployment.

#### Scenario: 429 triggers failover

WHEN a deployment answers 429
THEN the request is retried on the next eligible deployment without surfacing
the error to the client.

The gateway SHALL detect 429, quota exhaustion, and upstream failures and
retry the request on the next eligible deployment.

Once a content-bearing SSE event has been sent to the client, the gateway
SHALL NOT retry that request on another deployment.

#### Scenario: Mid-stream failure surfaces

WHEN the upstream fails after content events were already forwarded
THEN the error is surfaced to the client instead of restarting elsewhere.

Once a content-bearing SSE event has been sent to the client, the gateway
SHALL NOT retry that request on another deployment.

The gateway SHALL cool down a deployment after rate-limit or quota errors —
including ones that arrive after a stream has started — and skip it while it
cools down.

#### Scenario: Post-stream 429 cools down

WHEN a stream ends with a rate-limit error
THEN the deployment is skipped for new requests until its cooldown ends.

The gateway SHALL cool down a deployment after rate-limit or quota errors —
including ones that arrive after a stream has started — and skip it while it
cools down.

The gateway SHALL reduce 429s proactively: secure additional paths, pace
requests, and pre-empt deployments before their limits hit, rather than only
reacting to errors.

#### Scenario: Pre-empt before the cap

WHEN a deployment's remaining limit approaches zero
THEN new requests are routed elsewhere before a 429 occurs.

The gateway SHALL reduce 429s proactively: secure additional paths, pace
requests, and pre-empt deployments before their limits hit, rather than only
reacting to errors.

### Requirement: Context-length rejection

On context overflow the gateway SHALL NOT fall back to a different or
larger-context model; it SHALL return a clear OpenAI-shaped 400
`context_length_exceeded` before calling upstream, using a pre-send token
estimate against the deployment's context size.

#### Scenario: Opt-in truncation

WHEN the caller opts into middle-out truncation
THEN the gateway keeps the system message and latest turns, drops middle
content, and reports the dropped count in a response header. Rejection stays
the default.

The gateway SHALL be designed to not go down: patches deploy with zero
downtime and no single failure takes the service offline.

#### Scenario: Zero-downtime patch

WHEN a new build is deployed
THEN in-flight requests finish and new requests keep being served without an
outage window.

The gateway SHALL be designed to not go down: patches deploy with zero
downtime and no single failure takes the service offline.

The gateway SHALL stay stable under high request volume with minimal
overhead per request.

#### Scenario: Sustained high RPS

WHEN request volume stays high for an extended period
THEN latency and error rates stay within target without resource exhaustion.

The gateway SHALL stay stable under high request volume with minimal
overhead per request.
