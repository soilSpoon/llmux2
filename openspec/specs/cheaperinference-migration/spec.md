# cheaperinference-migration

## Purpose

Moving CheaperInference traffic from OmniRoute onto llmux2.

## Requirements

### Requirement: Exact API compatibility

The gateway SHALL match the request arguments and response shapes
CheaperInference requires, exactly.

#### Scenario: Existing client unchanged

WHEN a CheaperInference client switches its endpoint from OmniRoute to llmux2
THEN its requests and parsed responses work without code changes.

### Requirement: Complete migration before switchover

All OmniRoute traffic SHALL be migrated, with nothing dropped, before
llmux2 takes over; real OmniRoute traffic is the reference for coverage.

#### Scenario: Coverage checked against OmniRoute logs

WHEN comparing OmniRoute's routed models against llmux2's coverage
THEN every model and request shape OmniRoute serves is mapped before the
switch.

### Requirement: Revenue-oriented allocation

Provider and capacity allocation SHALL favor increasing CheaperInference
revenue.

#### Scenario: Capacity added where it earns

WHEN choosing between two providers to expand
THEN the one serving higher-revenue models is preferred.

### Requirement: No OmniRoute investment

Work SHALL go into the llmux2 replacement, not into improving the OmniRoute
server itself.

#### Scenario: OmniRoute bugs not fixed

WHEN an OmniRoute defect is found during migration
THEN it is worked around on the llmux2 side rather than patched upstream.
