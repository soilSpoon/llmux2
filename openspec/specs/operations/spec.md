# operations

## Purpose

Operability and the engineering standards the project holds itself to.

## Requirements

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

### Requirement: Autonomous merge gate

Devin bots SHALL merge only after the merge gate verifies an open, non-draft,
conflict-free PR targeting the default branch, successful required head checks,
no failed head checks or unresolved review threads, a head-specific Devin Review,
no blocking latest reviews, and a passing head-specific independent session attestation.
Verification comments SHALL come from a policy-allowlisted account.
Required checks SHALL come from the checked-in ruleset and review markers from
the merge policy. Fetch failures SHALL block merging.

#### Scenario: New commit invalidates verification

WHEN a PR head changes after review or independent verification
THEN the gate rejects stale evidence and merging requires fresh evidence.

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
