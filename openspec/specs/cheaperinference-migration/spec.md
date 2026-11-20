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

### Requirement: Explicit Workers AI import

The importer SHALL require both `--cloudflare-account-id-file` and
`--cloudflare-token-file` (or their API equivalents) before replacing
Cloudflare bridge legs with official Workers AI deployments. It SHALL read
only the non-secret, validated account ID to construct the account URL and
SHALL reference the token as an absolute `file:` credential without opening
or copying it, including during real-loader validation.

#### Scenario: Registry aliases share canonical connections

WHEN opted-in legs use `cfpd` or `cfp`
THEN they use eligible `cloudflare-direct` or `cloudflare-playground`
connections respectively, with reviewed exact `@cf/` upstream IDs.

#### Scenario: Unsupported or unapproved Workers AI route

WHEN opt-in is absent or a model is unknown, deprecated, or excluded
THEN no Workers AI deployment is emitted for that leg and a warning explains
the omission, including for `@cf/meta/llama-3.2-1b-instruct`.

#### Scenario: Workers AI quota is not a spending guarantee

WHEN a Workers AI deployment is emitted
THEN it has `rpm = 1`, and documentation warns that this is per deployment,
not account-wide neuron accounting or a guarantee against paid overage.

### Requirement: Free-only keyed Kilo import

The importer SHALL keep keyed Kilo deployments keyed and SHALL require an
operator-supplied `--kilo-free-models` pricing snapshot with both prompt and
completion prices exactly zero. Missing, conflicting, or nonzero prices
SHALL fail closed. Anonymous Kilo behavior SHALL remain unchanged.

#### Scenario: External Kilo key

WHEN `--kilo-key-file` is supplied
THEN its contents are not opened or copied; only a `file:` credential is
emitted, and model eligibility still requires the pricing snapshot.

#### Scenario: Static pricing is not live enforcement

WHEN the operator supplies a snapshot
THEN documentation identifies the snapshot as static evidence rather than
live cost enforcement; paid models and missing pricing do not become keyed
deployments through names or free-status hints alone.

### Requirement: Reviewed official OpenRouter replacements

With `--openrouter-replacements`, the importer SHALL route only reviewed
OpenRouter model IDs through eligible connected official endpoints or
opted-in Workers AI, using exact source-linked upstream IDs. It SHALL NOT
retain original OpenRouter or unverified mirror fallback legs on those names.
Unknown, unverified, and new-paid-key replacements SHALL be omitted with
warnings. Without the flag, existing OpenRouter routing SHALL be preserved.

#### Scenario: Replaced combo identity remains usable

WHEN an OpenRouter-named combo has an official replacement
THEN its name and retained ID remain resolvable by nested combo references,
`combo/` aliases, and model-combo mappings, using only the replacement.

#### Scenario: Unavailable winning mapping

WHEN the highest-precedence mapping targets a missing, inactive, or
deployment-empty combo
THEN its name and dependent aliases remain unavailable, even if a seeded
official replacement or lower-precedence mapping exists.

#### Scenario: Transitive mapping availability

WHEN clearing an unavailable winning mapping empties nested mapping targets
THEN replacement and combo availability is recomputed to a fixed point before
alias and mapping preparation, so dependent replacement names stay unavailable.

#### Scenario: Source connection restrictions

WHEN an OpenRouter leg has `connectionId` or nonempty `allowedConnectionIds`
THEN it is not rewritten onto an unrelated replacement connection.

#### Scenario: Compatibility and rollback

WHEN an operator needs the previous Cloudflare or OpenRouter policy
THEN removing their opt-in flags restores the previous replacement-only or
source-routing behavior. Keyed Kilo remains fail-closed without pricing
metadata; batch replacements are synchronous and preserve neither batch
scheduling nor batch discounts.
