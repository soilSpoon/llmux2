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

### Requirement: Optional OmniRoute cutover fallback

The importer SHALL accept `--omniroute-fallback` and `--omniroute-key` together
or neither. The key SHALL be a nonempty `env:` or `file:` reference and SHALL
remain a reference in generated TOML. The URL SHALL obey gateway base URL
validation: HTTPS, or HTTP only for loopback, without credentials, query, or
fragment. Generated configuration SHALL pass the existing runtime config parser.
Without these flags, existing import output SHALL remain byte-identical.

#### Scenario: Fallback leg is last

WHEN fallback flags are provided for an unrestricted non-Codex imported model
THEN its OmniRoute deployment has priority strictly greater than every native
deployment, a unique ID, and an upstream model equal to the llmux model name.

#### Scenario: Codex and allowed-key models get no fallback leg

WHEN an imported model can reach Codex or has `allowed_keys`
THEN the importer adds no OmniRoute deployment to that model.

#### Scenario: Codex IDs excluded from catch-all

WHEN fallback flags are provided
THEN the importer emits a `*` model with `upstream_model = "*"`
AND excludes Codex provider aliases and node prefixes, every Codex-reaching
combo and model alias, enabled mapping patterns that can reach Codex, and the
names and aliases of every allowed-key model, even if Codex connections were
not imported.

#### Scenario: Unsafe mapping cannot be expressed

WHEN an enabled Codex-routing mapping pattern cannot be expressed as an exact
ID or a prefix with a single trailing `*`
THEN import fails clearly without writing the output configuration.

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
