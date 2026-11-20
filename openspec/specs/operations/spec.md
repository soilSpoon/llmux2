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

### Requirement: Autonomous merge gate

Devin bots SHALL merge only after the merge gate verifies an open, non-draft,
conflict-free PR targeting the default branch, successful required head checks,
no failed head checks or unresolved review threads, a head-specific Devin Review,
no blocking latest reviews, and a passing head-specific independent session attestation.
Verification comments SHALL come from a policy-allowlisted account.
Devin Review evidence SHALL be either a head-specific review from the Devin app
containing a configured real-review body marker, or a successful latest head
commit status for the configured context created by that app. A latest pending,
failed, or errored status SHALL block even if a marked review exists.
Author thread replies SHALL NOT satisfy that evidence.
Statuses from other creators SHALL be ignored before selecting the latest Devin status.
The configured commit status context SHALL be matched case-insensitively.
Empty-body COMMENTED reviews SHALL NOT supersede a reviewer's blocking review.
Only the latest run of each head check name SHALL determine its outcome.
Every latest head check SHALL be completed before merging.
Required checks SHALL come from the checked-in ruleset and review markers from
the merge policy. Fetch failures SHALL block merging.

#### Scenario: New commit invalidates verification

WHEN a PR head changes after review or independent verification
THEN the gate rejects stale evidence and merging requires fresh evidence.

#### Scenario: Repository policy tools

WHEN CI checks PR policy or the coverage ratchet
THEN it SHALL run `cargo xtask pr-policy` or
`cargo xtask coverage-ratchet <coverage.json>` respectively, preserving the
existing environment inputs and success/failure exit codes.
AND the non-default `xtask` workspace member SHALL expose one binary with a
module per tool, independent of gateway crates.

### Requirement: Fast feedback loops

CI/CD and tests SHALL stay fast: only needed steps, no unnecessary work, since
a long loop slows management, improvement, and implementation.

#### Scenario: New CI step justified

WHEN a workflow step is proposed
THEN it is kept only if it gates a real failure mode; anything else is
dropped.

#### Scenario: Policy cache and test ownership

WHEN PR policy and coverage build xtask
THEN PR policy SHALL use a separate small dependency cache rather than restoring
the coverage job's workspace artifacts, and coverage SHALL keep its default Rust
cache. xtask tests SHALL run in the coverage job rather than again in check.

### Requirement: Rust benchmark tools

The non-default `llmux-bench` workspace member SHALL provide `mock-upstream`,
`compare-gateways`, and `live-reliability` subcommands. `cargo xtask bench` SHALL
delegate to the release benchmark binary without depending on benchmark or
gateway crates. Benchmark tests SHALL use local fake upstreams, not providers.

#### Scenario: Comparable fixed-rate measurements

WHEN measuring 1,000 RPS gateway overhead
THEN the harness SHALL discard one direct/gateway warm-up pair, alternate timed
route order, and compare the median paired p99 delta to the strict 1 ms target.

#### Scenario: Safe comparison and reliability reports

WHEN comparing two gateways or injecting reliability faults
THEN the Rust tools SHALL preserve request caps, secret-reference inputs,
redaction, offline mode, and JSON/HTML/Markdown report contracts.

### Requirement: Simple and extensible

The codebase SHALL stay simple with low overhead, and be easy to extend and
maintain. Code that does not serve the requirements should be removed or
merged.

#### Scenario: Dead path pruned

WHEN a module no longer serves a requirement
THEN it is removed or folded into the code that replaces it.
