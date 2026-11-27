# provider-quota

## Purpose

How llmux2 classifies upstream exhaustion, what scope a cooldown covers, and
how quota exhaustion differs from models a credential is not allowed to serve.
The goal: a credential or quota family that is exhausted parks itself until its
documented reset instead of burning attempts, and unrelated legs keep serving.

## Requirements

### Requirement: Three exhaustion classes

Upstream failures SHALL classify into three classes with distinct handling:

- `RateLimited` — transient per-model or per-minute limits; cools the
  deployment only, honoring a `Retry-After` hint else exponential backoff.
- `QuotaExhausted` — a billing-period or quota-window exhaustion; cools the
  quota scope (below) until the reported or derived reset.
- `NotProvisioned` — the credential's plan does not cover the model (e.g.
  morph "no API allowance"); disables that deployment until config reload,
  never retries, and never cools the credential — other models on the same
  credential are unaffected.

#### Scenario: Per-minute vs per-day

WHEN a Google QuotaFailure reports a per-minute violation
THEN the deployment cools briefly; when it reports `perDay`, the quota scope
cools until the provider's reset.

#### Scenario: Not provisioned

WHEN morph answers "no API allowance" for an out-of-plan model
THEN that deployment stops receiving candidates immediately while the morph
credential keeps serving in-plan models.

### Requirement: Cooldown scope is (credential, quota family)

Exhaustion SHALL park the quota window that actually drained — identified by
credential plus the provider's quota family — not the whole credential and not
one deployment. Providers define families: antigravity reports per-family
weekly windows (`gemini_weekly`, `claude_gpt_weekly`, `argon_weekly`); a single
family's exhaustion leaves other families on the same account serving.

#### Scenario: Family isolation on antigravity

WHEN an antigravity account exhausts `gemini_weekly`
THEN its gemini legs cool until the weekly reset while its claude legs keep
serving on `claude_gpt_weekly` budget.

#### Scenario: Whole-account exhaustion

WHEN a provider's quota is account-wide (Cloudflare daily neurons, morph
monthly quota)
THEN the family is the account itself and every leg on the credential cools.

### Requirement: Authoritative resets may exceed the backoff cap

A provider-reported reset (`reset_at`, reset headers, or a documented fixed
boundary like Cloudflare's UTC-midnight reset) SHALL be honored up to
`cooldown.quota_max_ms` (default ~32 days), independent of the exponential
backoff cap `cooldown.max` that governs unhinted 429s.

#### Scenario: Monthly quota

WHEN morph reports monthly quota exhaustion
THEN the credential parks until the next monthly boundary instead of
re-probing every `cooldown.max`.

### Requirement: Provider signature catalog

Each adapter SHALL map its provider's exhaustion signatures to the classes
above. Current catalog, from observed traffic:

| Provider | Signature | Class | Reset |
| --- | --- | --- | --- |
| Cloudflare Workers AI | 429 with `errors[].code: 4006` | QuotaExhausted (account) | next UTC midnight |
| Gemini API | QuotaFailure `perDay` violations | QuotaExhausted | retryInfo delay / daily reset |
| Morph | "monthly quota exceeded" | QuotaExhausted (account) | next month UTC |
| Morph | "no API allowance" | NotProvisioned | config reload |
| Antigravity | 429 RESOURCE_EXHAUSTED | QuotaExhausted (family) | weekly window end |
| Credit relays | `insufficient_credits` / 402 credits | QuotaExhausted (account) | unknown; park to quota_max |
| IteraCompute | "capacity temporarily exhausted" | RateLimited | backoff |
| runcrate (free tier) | 429 code `free_allowance_used`, any case/separator | QuotaExhausted | reported reset hint, else derived wait |
| Generic | `Retry-After`, `x-ratelimit-remaining{,-requests}: 0` + reset | RateLimited / QuotaExhausted | header |
| IETF window | `x-ratelimit-{limit,remaining,reset}` (AI Horde) | RateLimited / parked on `remaining: 0` | epoch or delta-seconds header |

When OpenAI-scoped and generic window headers are mixed, the generic
`x-ratelimit-remaining` SHALL be only the fallback request counter: an
explicit healthy `x-ratelimit-remaining-requests` must not be overridden, so
an exhausted scoped token window still selects `x-ratelimit-reset-tokens`.

New signatures SHALL be added here with the code change that parses them.

### Requirement: Multi-account providers stay independent

Sibling credentials for one provider (gemini keys, antigravity accounts) SHALL
cool independently; exhausting one account never parks another.

#### Scenario: Five gemini keys

WHEN one gemini key hits its daily limit
THEN its legs cool while the other four keys keep serving identical
deployments.

### Requirement: Quota state is observable

Cooldown state SHALL expose scope and reset time (existing
`cooldown_ms` per deployment, plus the cooled family/credential) so monitoring
and alerting report "resets at T" rather than a generic failure.

### Requirement: No billed legs behind free-only paths

A credential whose plan covers only a subset SHALL declare the covered exact
upstream ids in `allowed_upstream_models`. Entries SHALL be nonempty, unique,
exact ids — never `*` — and llmux-config SHALL reject every deployment on that
credential whose `upstream_model` is absent from the list. Because the match is
exact and static, a catch-all deployment cannot use such a credential. Anything
out-of-plan relies on `NotProvisioned` only as a safety net, not as the primary
control. The `x-llmux-free-only` request flag SHALL keep excluding `billed`
legs.

#### Scenario: Morph plan boundary

WHEN the morph credential's reviewed Ultra scope covers Kimi K3.1
(`morph-kimik31`) and the DeepSeek V4.1 Flash variants (`morph-dsv41flash`,
`morph-dsv41flash-1017`, and privileged-access `morph-dsv41flash-abliterated`)
THEN a deployment on that credential naming `morph-kimik3`,
`morph-kimik3-fast`, `morph-kimik3-abliterated`, or any other out-of-scope
upstream model fails config validation, while the listed Kimi K3.1 and
DeepSeek ids load. Although the Ultra prose calls `morph-kimik3` a K3.1
alias, Morph's machine-readable catalog identifies it as `moonshotai/Kimi-K3`
rather than `moonshotai/Kimi-K3.1-Pro`; exact whitelisting SHALL prefer the
canonical `morph-kimik31` id over that ambiguous alias.
