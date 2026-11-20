# operations

## Purpose

Operability and the engineering standards the project holds itself to.

## Requirements

### Requirement: Standalone slot deployment tools

The non-default `llmux2-ops` workspace package SHALL build standalone
`llmux2-deploy` and `llmux2-guard` binaries installed under `$HOME/llmux2/bin`.
Production hosts SHALL NOT require Python or Cargo for deployment or recovery.
The tools SHALL serialize through the same deployment lock, use private atomic
file replacement, preserve the source and installed configuration checks,
observe seven stable unit samples at half-second intervals, and try health up
to sixty times before stopping the old slot. The health probe SHALL use a
two-second TCP connect/read timeout and accept only HTTP 200 whose first up-to-three
decoded body bytes equal exactly `ok`, respecting Content-Length and chunked
framing. systemctl and configuration checks SHALL have 130- and 120-second
deadlines respectively.

#### Scenario: Failed handoff before old-slot stop

WHEN the new slot fails to start, remain stable, or become healthy
THEN deployment SHALL stop the new slot and leave the old slot unchanged,
reporting cleanup failures separately.

#### Scenario: Recovery and late handoff errors

WHEN neither slot is active and the recorded listen address is unhealthy
THEN the guard SHALL start the recorded slot.
WHEN deployment reaches old-slot stop and a subsequent operation fails
THEN it SHALL leave the new slot running and report that handoff stage.

#### Scenario: Deployment test ownership

WHEN CI runs deployment tests
THEN it SHALL run the Rust tests once in the coverage job, using fake waits,
and merge-gate Rust tests SHALL also run there rather than in the check job.

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

#### Scenario: Rust merge preflight

WHEN a bot evaluates whether PR N can merge
THEN it SHALL run `cargo xtask merge-gate N` from a trusted, up-to-date main
checkout, using authenticated `gh api` with REST and review-thread pagination.
AND it SHALL exit 0 only on pass, report the evaluated SHA, reject a head change
during fetching, and perform no mutations. Bootstrap gate changes SHALL require
independent review before using the changed gate.

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

||||||| parent of d4d160c (Add authenticated /admin management API with hot config reload)
=======
### Requirement: Management over one shared core

Operators SHALL be able to inspect and control the running gateway through an
authenticated HTTP admin API, a CLI, and an MCP server. The CLI and MCP server
SHALL be thin clients of the same admin API and contain no routing or config
logic of their own. Admin endpoints SHALL exist only when
`server.dashboard_key` is configured and SHALL authenticate with that key as a
Bearer token using the same constant-time check as the dashboard. Secret
material SHALL never appear in any management response.

#### Scenario: Same answer everywhere

WHEN an operator asks for deployment state via the API, the CLI, or an MCP
tool call
THEN all three report the same data because the latter two call the same API.

#### Scenario: No key, no admin surface

WHEN `server.dashboard_key` is not configured
THEN `/admin` routes do not exist.

### Requirement: Runtime management operations

The admin API SHALL expose gateway status (uptime, version, counters),
deployment state (provider, priority, limits, disabled, cooldown, in-flight,
counters), credential state (id, kind, provider — never secrets), and the
canonical model map. It SHALL allow enabling or disabling a credential's
routing state, which affects every deployment drawing on it.

#### Scenario: Disable a provider's key

WHEN an operator disables a credential
THEN new requests skip every deployment that uses it until it is re-enabled.

### Requirement: Validated hot config reload

The admin API SHALL offer `POST /admin/reload` that re-reads the config file,
validates and rebuilds the executor, and swaps it in atomically. A failed
reload SHALL leave the running config untouched and report the error. The
request path SHALL stay overhead-free: one atomic read per request, no locks.

#### Scenario: Bad config rejected safely

WHEN the new config fails validation
THEN the reload reports the error and the old config keeps serving.

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
