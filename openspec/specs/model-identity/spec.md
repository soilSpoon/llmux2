# model-identity

## Purpose

How llmux2 names models so that the same upstream model reachable through many
providers and provider-specific name spellings is one logical model, while
names that merely look similar but denote different models stay distinct.

The canonical name is the name cheaperinference clients request. All other
spellings for the same model are aliases on that model entry. This spec pins
the rules; generated configs and the importer follow them.

## Requirements

### Requirement: Canonical names come from cheaperinference vocabulary

When several gateway names resolve to the same upstream model, the model
entry's `name` SHALL be the spelling cheaperinference uses when one exists in
that set. Provider-prefixed spellings (`factory/`, `numes/`, `anthropic/`,
`openrouter/`, node-id prefixes like `openai-compatible-chat-<uuid>/`) SHALL be
`aliases`, never the canonical name, when a cheaperinference name exists.

#### Scenario: Claude alias fan-in

WHEN `claude-sonnet-5.5`, `anthropic/claude-sonnet-5-5`,
`factory/claude-sonnet-5-5`, and `cantrust/anthropic/claude-sonnet-5.5` denote
the same upstream model
THEN one model entry named `claude-sonnet-5.5` carries the rest as aliases and
pools all of their deployments.

#### Scenario: No cheaperinference name

WHEN a cluster of equivalent names has no cheaperinference spelling
THEN the canonical name is the bare (unprefixed) spelling; if none exists, the
shortest spelling.

### Requirement: Deployments pool across providers under one name

Deployments from every name in an identity cluster SHALL merge onto the single
model entry, deduplicated by (credential, upstream_model). Priority and billing
flags survive the merge unchanged.

#### Scenario: Multi-provider model

WHEN `qwen3-14b` exists as `numes/qwen/qwen3-14b` and
`aihorde/koboldcpp/Qwen3-14B`
THEN the merged entry offers deployments on every credential that can serve
the model, and failover may cross providers.

### Requirement: Suffixes that change the model stay distinct

Names differing only by a meaningful suffix SHALL NOT merge. Meaningful
suffixes include reasoning effort (`-low`, `-medium`, `-high`, `-xhigh`),
precision/quantization (`-fp8`, `-int4`), tuning (`-instruct`, `-it`),
recency (`-preview`, dated `-YYYYMMDD`), billing markers (`-free`, `:free`,
`:batch`), and thinking toggles (`no-think/`).

#### Scenario: Effort variants are separate models

WHEN `claude-opus-5-5-low` and `claude-opus-5-5` both exist
THEN they remain separate model entries because they request different
reasoning effort from the upstream.

#### Scenario: Free-suffixed entry stays separate

WHEN `qwen3-14b-free` and `qwen3-14b` both exist
THEN they remain separate entries; the `-free` entry may carry only
verified-free deployments.

#### Scenario: Suffixed mapping stays unmerged at import

WHEN a `modelComboMappings` row points a spelling carrying an effort,
`-fast`, or `no-think`/`nothink`/`thinking` suffix at a combo whose leaf
does not carry the same suffixes
THEN the importer does not attach the spelling as an alias, emits no model
entry for it, and warns, so a request for it reaches the `*` catch-all,
which forwards the literal suffixed id to the real suffixed upstream.

#### Scenario: Matching suffix stays an alias

WHEN a mapping points `factory/claude-fable-5-low` at the `claude-fable-5-low`
combo
THEN the spelling attaches to that model's aliases normally.

### Requirement: Identity mapping is reviewable data

The name-cluster → canonical assignment SHALL be produced by a tool or import
step that emits a diffable mapping file, not silent inline merging, so a human
can correct over-merges before deploy.

#### Scenario: Over-merge correction

WHEN the tool groups two names an operator knows are different models
THEN editing the mapping file and re-running produces separate entries without
code changes.

#### Scenario: Canonical map regeneration

WHEN `llmux-import` runs with `--canonical-map <path>`
THEN it writes `canonical-map.json` mapping every emitted model name to the
spellings resolving to it; a spelling kept distinct by the suffix rule
appears as its own entry keyed by itself.

### Requirement: Name resolution costs O(1) on the request path

Canonical and alias resolution SHALL happen through a precomputed lookup built
at config load. No per-request pattern matching or catalog scans are allowed
on the hot path.

#### Scenario: Alias lookup

WHEN a request names an alias
THEN the model resolves through the same hash lookup as a canonical name,
adding no measurable latency.
