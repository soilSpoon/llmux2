# cheaperinference-migration

## Purpose

Moving CheaperInference traffic from OmniRoute onto llmux2.

## Requirements

### Requirement: DeepSeek parameter compatibility

The importer SHALL configure built-in DeepSeek deployments to strip seed, user,
safety_identifier, and reasoning.effort through the core strip_request_fields
option. It SHALL preserve logprobs and top_logprobs, and leave other providers
and custom nodes unchanged.

#### Scenario: DeepSeek combo or direct route

WHEN a route expands onto a built-in DeepSeek connection
THEN the generated deployment loads with exactly those four stripped fields.

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

#### Scenario: Retired Llama bridge target

WHEN an opted-in Cloudflare bridge leg targets `meta/llama-3.1-8b-instruct`
THEN it uses the reviewed current 3B `@cf/ibm-granite/granite-4.0-h-micro`
instead of sending the retired upstream ID. Documentation SHALL disclose the
checkpoint and size change. Other excluded deprecated models remain excluded.

#### Scenario: Optional Workers AI snapshot

WHEN `--workers-ai-models` or its API equivalent supplies a successful
Cloudflare model-search response
THEN every Workers AI deployment SHALL require its mapped upstream name to
appear in the snapshot as well as the static allowlist; absent names SHALL be
skipped with warnings. An empty result permits no Workers AI deployments.
Unreadable, malformed, or failed API responses SHALL abort import without
echoing their contents. Without a snapshot the static policy remains in effect.

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

#### Scenario: Explicit OmniRoute fallback cannot bypass replacement policy

WHEN `--omniroute-fallback` and `--openrouter-replacements` are both enabled
THEN Codex exclusions are computed from the original source combos before
rewriting, and OpenRouter namespaces, source aliases, mappings, and nested
routes receive no OmniRoute fallback deployment and are excluded from the
catch-all, whether their replacement was accepted or rejected.

#### Scenario: Compatibility and rollback

WHEN an operator needs the previous Cloudflare or OpenRouter policy
THEN removing their opt-in flags restores the previous replacement-only or
source-routing behavior. Keyed Kilo remains fail-closed without pricing
metadata; batch replacements are synchronous and preserve neither batch
scheduling nor batch discounts.
