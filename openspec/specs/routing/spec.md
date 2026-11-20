# routing

## Purpose

How llmux2 chooses a deployment for a canonical model when several could serve
it, including quota-aware priority and cost policy.

## Requirements

### Requirement: Canonical model names

The gateway SHALL expose public canonical model names, each mapped to one or
more provider deployments with provider-specific model IDs.

### Requirement: Eligibility before ranking

The gateway SHALL skip a deployment that cannot serve right now — in cooldown,
reporting zero remaining limit, or at its concurrency cap — before applying
priority ordering.

#### Scenario: Cooling-down deployment skipped

WHEN the top-priority deployment is in cooldown
THEN the request goes to the next eligible deployment regardless of priority.

### Requirement: Quota-expiry-first priority

Among eligible deployments the gateway SHALL order by:

1. Quota that expires: sooner expiry first, since unused allowance is lost.
2. Refill period: shorter refill cycles before longer ones.
3. Free before paid when the above are equal.

#### Scenario: Expiring quota preferred

WHEN deployment A's quota expires in 1 hour and deployment B's resets daily
THEN A is tried first.

### Requirement: Remaining-amount scoring

The gateway SHALL score expiring-quota deployments by remaining amount
relative to time until expiry, not expiry time alone.

#### Scenario: Nearly empty quota deprioritized

WHEN a deployment expires soon but has almost no quota left
THEN a fuller deployment with a slightly later expiry may outrank it.

### Requirement: Concentrate on the best route

The gateway SHALL send traffic to the top-ranked deployment until it nears its
limits, and only then spill to the next. Load spreading is a safety valve for
avoiding rate-limit and latency spikes, not a goal in itself.

#### Scenario: Spillover near the cap

WHEN the top deployment approaches its request or concurrency limit
THEN overflow requests go to the next-ranked eligible deployment.

### Requirement: Quality floor

The gateway SHALL deprioritize a deployment whose observed latency or error
rate degrades, even for the same model.

### Requirement: Paid deployments last

The gateway SHALL use paid deployments only when free and subscription quota
is exhausted, and only within a configured cost cap.

### Requirement: Revenue-aware capacity

For deployments that also serve CheaperInference, the gateway SHALL preserve
quota for models that sell well instead of spending it on low-value traffic.
