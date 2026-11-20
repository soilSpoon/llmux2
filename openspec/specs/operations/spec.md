# operations

## Purpose

Operability and the engineering standards the project holds itself to.

## Requirements

### Requirement: Bounded fallback traffic visibility
The gateway SHALL expose reporting-only deployment `fallback` flags, defaulting
to false and always true for catch-all deployments. The importer SHALL mark
generated OmniRoute fallback deployments without changing no-flag output.
The authenticated dashboard SHALL expose fallback routed requests, all routed
requests and their ratio (zero for no traffic), using upstream responses ready
including first stream events, not failed attempts or completed transfers.
It SHALL retain at most 256 case-sensitive catch-all requested model IDs of at
most 256 UTF-8 bytes each, counting overflow separately. Dashboard UI SHALL show
fallback share, top retained IDs and fallback deployment labels.

#### Scenario: Known fallback and unknown traffic
- WHEN a known model uses a flagged deployment and an unknown ID uses catch-all
- THEN both count toward fallback share, but only the unknown ID enters the breakdown.

#### Scenario: Bounded tracking and errors
- WHEN retained IDs reach the cap or an ID exceeds the length bound
- THEN new IDs count in the other bucket while retained IDs continue accumulating.
- WHEN an upstream attempt fails before a response is ready
- THEN it does not increment served counts or the catch-all breakdown.


### Requirement: Korean dashboard

The gateway SHALL offer an at-a-glance dashboard in Korean, light theme by
default with a dark-mode switch.

#### Scenario: Theme toggle

WHEN the user toggles dark mode
THEN the dashboard switches theme and remembers the choice.

### Requirement: Enforced quality gates

TDD, reliability, and code-quality standards SHALL be enforced by CI gates and
tooling, not only by written rules.

#### Scenario: PR without tests fails policy

WHEN a PR changes `crates/*/src` without a test change or exemption
THEN the `pr-policy` check fails.

### Requirement: Fast feedback loops

CI/CD and tests SHALL stay fast: only needed steps, no unnecessary work, since
a long loop slows management, improvement, and implementation.

#### Scenario: New CI step justified

WHEN a workflow step is proposed
THEN it is kept only if it gates a real failure mode; anything else is
dropped.

### Requirement: Simple and extensible

The codebase SHALL stay simple with low overhead, and be easy to extend and
maintain. Code that does not serve the requirements should be removed or
merged.

#### Scenario: Dead path pruned

WHEN a module no longer serves a requirement
THEN it is removed or folded into the code that replaces it.
