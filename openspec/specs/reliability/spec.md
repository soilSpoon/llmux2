# reliability

## Purpose

Failure handling: failover, cooldowns, context limits, availability, and
deployment behavior under load.

## Requirements

### Requirement: Failover on limit and failure

The gateway SHALL detect 429, quota exhaustion, and upstream failures and
retry the request on the next eligible deployment.

### Requirement: Stream commitment

Once a content-bearing SSE event has been sent to the client, the gateway
SHALL NOT retry that request on another deployment.

### Requirement: Cooldowns

The gateway SHALL cool down a deployment after rate-limit or quota errors —
including ones that arrive after a stream has started — and skip it while it
cools down.

### Requirement: Rate-limit avoidance

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

### Requirement: Always available

The gateway SHALL be designed to not go down: patches deploy with zero
downtime and no single failure takes the service offline.

### Requirement: Stability under load

The gateway SHALL stay stable under high request volume with minimal
overhead per request.
