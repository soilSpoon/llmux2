# operations

## Purpose

Operability and the engineering standards the project holds itself to.

## Requirements

### Requirement: Deployment attribution

The gateway SHALL count failures and skips per deployment in its authenticated
deployment stats using fixed class and reason names, without provider error
bodies, account identifiers, or request data.
An expired deployment gets the `expired` skip reason, not a failed attempt.

#### Scenario: A routed request fails

WHEN a request fails after at least one deployment was attempted
THEN the gateway reports the final attempted deployment and total attempts
on the error response only when routing headers are enabled. A request that
fails before any deployment is attempted does not receive routing headers.

### Requirement: Standalone slot deployment tools

The non-default `llmux2-ops` workspace package SHALL build standalone
`llmux2-deploy` and `llmux2-guard` binaries installed under `$HOME/llmux2/bin`.
Production hosts SHALL NOT require Python or Cargo for deployment or recovery.
The tools SHALL serialize through the same deployment lock, use private atomic
file replacement, preserve the source and installed configuration checks,
observe seven stable unit samples at half-second intervals, and try health up
to sixty times before stopping the old slot. Deployment SHALL accept an
optional `--canary SECONDS` window: after the new slot is stable and healthy,
both slots SHALL keep running and share the SO_REUSEPORT listener while the
tool verifies the new unit keeps the same PID with zero restarts for the whole
window before the old slot stops. A canary failure SHALL stop the new slot and
leave the old slot serving. The health probe SHALL use a
two-second TCP connect/read timeout and accept only HTTP 200 whose first up-to-three
decoded body bytes equal exactly `ok`, respecting Content-Length and chunked
framing. systemctl and configuration checks SHALL have 130- and 120-second
deadlines respectively.

#### Scenario: Failed handoff before old-slot stop

WHEN the new slot fails to start, remain stable, or become healthy
THEN deployment SHALL stop the new slot and leave the old slot unchanged,
reporting cleanup failures separately.

#### Scenario: Canary window aborts before handoff

WHEN deployment runs with `--canary SECONDS` and the new slot restarts, stops,
or changes PID during that window
THEN the old slot SHALL remain active and recorded, and the new slot SHALL be
stopped.

#### Scenario: Recovery and late handoff errors

WHEN neither slot is active and the recorded listen address is unhealthy
THEN the guard SHALL start the recorded slot.
WHEN deployment reaches old-slot stop and a subsequent operation fails
THEN it SHALL leave the new slot running and report that handoff stage.

#### Scenario: Deployment test ownership

WHEN CI runs deployment tests
THEN it SHALL run the Rust tests once in the coverage job, using fake waits,
and merge-gate Rust tests SHALL also run there rather than in the check job.

### Requirement: Alert thresholds over gateway stats

`llmux2-ops` SHALL build a standalone `llmux2-alert` binary that probes the
running gateway and reports threshold breaches as `ALERT` lines with a nonzero
exit, so a systemd timer plus journal or an `OnFailure` hook surfaces them.
Each pass SHALL check `/healthz` and `GET /admin/deployments`, authenticating
with a bearer token read from a configured `key_file`. Unreachable health or
admin endpoints SHALL themselves be breaches.

`alerts.toml` (default `$HOME/llmux2/alerts.toml`, overridable with `--config`)
SHALL support `listen`, `key_file`, `failure_ratio`, `min_attempts`,
`max_cooldowns` and `max_disabled`; a missing file uses defaults (only
reachability checks). An unset threshold disables its check. `failure_ratio`
SHALL compare against the previous counter snapshot persisted atomically in
`alert-state.json`, counting `failed_attempts` plus `stream_errors` over new
attempts per deployment, and SHALL apply only when new attempts reach
`min_attempts` (default 10; `0` disables the floor). A counter decrease in
`routed`, `failed_attempts`, or `stream_errors` SHALL skip the ratio and
re-base the snapshot. A 200 response without a `deployments` array SHALL alert
and leave the snapshot untouched. `llmux2-alert` SHALL NOT take the deployment lock.

#### Scenario: Breach reports and exits

WHEN a configured threshold is exceeded or a probe fails
THEN `llmux2-alert` prints one `ALERT` line per breach and exits 1.

#### Scenario: First run and counter reset

WHEN there is no snapshot, or a deployment's counters decreased
THEN the run records a fresh baseline and reports no failure-ratio breach.

#### Scenario: Quiet window

WHEN new attempts are below `min_attempts`
THEN no failure-ratio breach fires regardless of the ratio.

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
### Requirement: Durable per-request routing records
When `server.request_log` names a path, the gateway SHALL append one JSON
object per routed request as a JSONL line. Each record SHALL carry the
timestamp, request ID, requested model, last-attempted deployment ID and
provider, attempt count, total latency, the outcome (`routed` or `error`),
the failure kind on errors, and the remaining RPM/concurrency headroom of
the last-attempted deployment. Each record SHALL also carry a fingerprint of
the gateway client key used (a `key_<16 hex>` hash keyed by the configured
key set, never the key itself), the count of image inputs in the inbound
request, and — when the upstream supplies one — the provider-side request ID
(such as an Anthropic `msg_`/`req_` ID) so a provider flag can be matched by
ID rather than timestamp alone. Optional fields SHALL be omitted rather than
written as null, and the file SHALL be
created on startup, failing startup when it cannot be opened. Records SHALL
contain no credentials or request bodies. When `server.request_log` is
unset, no file is written.

#### Scenario: Failed request still recorded
- WHEN a request exhausts every candidate
- THEN the record names the last-attempted deployment, the attempt count and `all_candidates_exhausted`.

#### Scenario: Provider flag traced by upstream ID
- WHEN an upstream response carries `anthropic-request-id`, `x-request-id` or `request-id`, or a provider-owned body carries `id` or `request_id` (including in an error body)
- THEN the record includes it as `upstream_request_id`, on success and on failure, preferring the header id when both differ.

#### Scenario: No attempts made
- WHEN every candidate is cooling or limited before any attempt
- THEN the record omits `deployment`, `provider` and `quota_remaining`.

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

### Requirement: Synthetic route probes

The `llmux2-ops` package SHALL provide a standalone `llmux2-probe` binary that
verifies the running gateway as a client sees it: `GET /healthz` and
`GET /v1/models` on every run, then a `POST /v1/chat/completions`
for `--per-run` canonical models (default 1) chosen by a rotation cursor in
`--state-file`, so each timer interval costs one synthetic provider request
per run rather than one per model. `--models` SHALL pin an explicit set and
`--all-formats` SHALL add one `/v1/responses` and one `/v1/messages` probe.
All three inference formats SHALL use an output token limit of 16 so providers
that reject smaller limits can be probed without request-shape failures.
The probe SHALL authenticate with the gateway client key from `--key-file`,
bound each request by `--timeout`, bound response bodies, print a JSON report
naming each check's status and latency, never print the key, and exit 0 only
when every probe passes (1 on failure, 2 on usage errors). A
`llmux2-probe.service`/`llmux2-probe.timer` pair under `deploy/systemd` SHALL
run it periodically as a oneshot user unit.

#### Scenario: Rotating coverage bounds provider cost

WHEN the timer fires repeatedly
THEN each run probes `--per-run` models in sorted rotation, persisting the
cursor atomically so every canonical model is covered over successive runs.

#### Scenario: Provider minimum token limit is respected

WHEN a route rejects output token limits below 16
THEN chat, responses, and messages probes send a limit of 16 rather than
failing because the probe requested too few tokens.

#### Scenario: Failure is observable

WHEN healthz, the models route, or a probed model returns a non-2xx answer
THEN the run exits 1 and the JSON report names the failing check with its
status, so the failed unit is visible in the journal.

The state file SHALL also accumulate a consecutive-failure streak per probed
model; a success clears it, and a key-restricted model leaves no evidence
either way. A model whose streak reaches `--dead-after` (default 2) of status
404 — a deterministic upstream refusal — SHALL be listed under `dead` in the
report, and `--dead-file PATH` SHALL write those names as a JSON array for
`llmux-import --exclude-models`. Transient statuses such as 429 cooldowns or
502 upstream errors SHALL never count toward dead.

#### Scenario: A persistently dead route is dropped at the source

WHEN a model's last `--dead-after` probes all returned 404
THEN it appears in the report's `dead` list and in `--dead-file`, so the next
import drops it rather than advertising it in `/v1/models`.

A dedicated low-rate sweep SHALL cover the whole catalog: `--sweep` SHALL
probe every listed model once in a single run without moving the rotation
cursor, and SHALL be rejected in combination with `--models`. The shipped
`llmux2-sweep.timer` SHALL run it on its own state file, feeding
`--dead-file`; `llmux2-prune.timer` SHALL re-run the import with
`--exclude-models` on a schedule and deploy the regenerated config so dead
routes are dropped without manual pruning. The scheduled import SHALL carry
the site's supplemental importer options, the deployed slot's listen
address, and the site's overlay (`--overlay`, merging `[server]`/`[routing]`
settings and site-added credentials/models), and SHALL never delete secrets
the deployed config still references. The prune SHALL shred the credential
export on every exit — success, failure, or abort — and deploy the installed release binary.

#### Scenario: A full sweep finds the long tail

WHEN `--sweep` runs against a catalog of N models
THEN every listed model is probed exactly once and the rotation cursor is
unchanged, so recurring probes keep their place.

### Requirement: Simple and extensible

The codebase SHALL stay simple with low overhead, and be easy to extend and
maintain. Code that does not serve the requirements should be removed or
merged.

#### Scenario: Dead path pruned

WHEN a module no longer serves a requirement
THEN it is removed or folded into the code that replaces it.
