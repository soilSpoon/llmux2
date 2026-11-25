# upstream-export

## Purpose

The `llmux export-upstream-quota` command produces a local, credential-free
JSON inventory of upstream supply for profitability and capacity monitoring.
It joins static config inventory, optional durable request-log demand, and an
optional `/admin/deployments` JSON snapshot without contacting production or
changing gateway state.

## Requirements

### Requirement: Offline export command

The CLI SHALL provide `export-upstream-quota --config PATH` with optional
`--request-log PATH`, `--runtime-deployments PATH`, `--now RFC3339`, and
`--no-request-log` arguments. When `--request-log` is omitted and
`--no-request-log` is not set, the export SHALL use `server.request_log` from
the config when present. The command SHALL write one
JSON document to stdout and SHALL NOT write gateway state, call upstream
providers, or require admin authentication. The document SHALL identify input
file paths, modification times when locally available, and whether the request
log was provided, taken from config, absent, or disabled by flag.

#### Scenario: Offline export

WHEN the command receives a config, request log, runtime deployment snapshot,
and deterministic `--now`
THEN it prints rows keyed by canonical model name, with aliases contributing to
that model's demand totals.

### Requirement: Credential-free output

The export SHALL contain deployment ids, credential ids, provider names,
upstream model ids, priorities, billing classes, quota metadata, demand counts,
and availability explanations. It SHALL NOT contain credential values, secret
references, request bodies, client keys, or provider error bodies.

#### Scenario: Secret-bearing config

WHEN the input config contains `env:` or `file:` credential references
THEN the export uses only non-secret inventory metadata and does not echo those
references.

### Requirement: Unknown values are explicit

A value that cannot be derived from the supplied inputs SHALL be `null` (or an
empty array only when the input proves there were no events), and the row SHALL
explain the reason in `availability`. The exporter SHALL NOT estimate unobserved
provider capacity, invent paid marginal costs, or report zero as a free/membership
opportunity cost when the next-best use of the same quota is unknown.

#### Scenario: Runtime state absent

WHEN no runtime deployment snapshot is supplied
THEN runtime-only parking and cooldown facts are `null` or listed as partial in
`availability`, while config-disabled credentials still mark their legs parked.

### Requirement: Quota usage reconstruction is coverage-bound

Request-denominated quota usage SHALL be reconstructed by counting upstream leg
sends in the request log inside each configured sliding or fixed window.
`remainingUnits` for a window SHALL be emitted only when the log's first record
predates the window start; otherwise the window exposes observed usage but null
remaining units. Token-denominated windows SHALL expose configured capacity but
null remaining units because the request-log contract does not preserve the
admission-time token estimate.

#### Scenario: Partial log window

WHEN a daily window starts at midnight but the first available log record is
from 09:00
THEN that window's `usedUnits` may be reported with partial coverage, while its
`remainingUnits` is `null` rather than overestimated.

### Requirement: Row-level capacity is conservative

Row-level `remainingUnits` SHALL be the largest fully reconstructable
request-unit remaining value available to a single leg, not a sum across legs;
overlapping credential or shared-quota scopes are not double-counted.
`capacityUnitsPerWindow` and `capacitySaturation` SHALL describe the longest
request-denominated window on that same leg.

#### Scenario: Multiple independent legs

WHEN two legs each have ten remaining requests
THEN row-level `remainingUnits` is ten as a conservative lower bound rather
than twenty, because shared scopes cannot always be allocated exactly from the
export inputs.

### Requirement: Failure fields separate gateway and upstream signals

Demand and latency fields SHALL count buyer-visible request outcomes from the
request log, including `all_candidates_exhausted` in `lastErrorKind` and
failure latency. `recent429` SHALL count upstream leg outcomes of
`rate_limited` or `quota_exhausted`, including a limited leg later rescued by
failover, and SHALL NOT use gateway-level `all_candidates_exhausted` as an
upstream 429/402 observation.

#### Scenario: Failover after a leg limit

WHEN one leg returns `rate_limited` and another leg serves the request
THEN the row is counted as served and the leg limit still appears in
`recent429`.

### Requirement: Traffic classes stay separated

When request-log input is available, demand counts SHALL be emitted both as all-key totals and per `(trafficClass, clientKeyFingerprint)` buckets. Fingerprints SHALL use the gateway's non-secret `key_<16 hex>` form. Classification SHALL come from an explicit `--traffic-classes` mapping; unmapped or missing keys SHALL be `unknown`, not inferred by the exporter. Buyer-shaped, synthetic-probe, internal, and unknown traffic SHALL NOT be collapsed into one buyer-demand count.

#### Scenario: Probe traffic separated

WHEN one fingerprint is classified as `synthetic_probe` and another as `ci_buyer_shaped`
THEN `trafficBreakdown` exposes separate attempted, served, and zero-candidate counts for both, and the row-level aggregate remains visibly all-traffic rather than buyer-only.

### Requirement: Billing declaration is not plan truth

`billing` in config SHALL be emitted only as `billingDeclared`. Leg-level `freeOrMembership` SHALL carry `classificationSource` and `classificationConfidence`. Antigravity OAuth legs on `daily-cloudcode-pa.googleapis.com` SHALL be marked membership only as inferred provenance unless independently verified; unverified billed declarations SHALL remain unknown rather than being treated as proven paid supply.

#### Scenario: Antigravity subscription asset

WHEN an antigravity OAuth leg is declared `billed` in config but uses the subscription/free-quota host
THEN the export reports `billingDeclared=billed`, `freeOrMembership=membership`, and inferred provenance rather than trusting the config label.
