# routing

## Purpose

How llmux2 chooses a deployment for a canonical model when several could serve
it, including quota-aware priority and cost policy.

## Requirements

### Requirement: Canonical model names

The gateway SHALL expose public canonical model names, each mapped to one or
more provider deployments with provider-specific model IDs.

#### Scenario: Two deployments behind one name

WHEN canonical model `m` maps to deployments on two providers with different
upstream model IDs
THEN a client requesting `m` is served by whichever deployment routing selects.

### Requirement: Same-model serving only

Every deployment under one canonical model SHALL serve that same model.
Same-family variant upstream IDs (`A-1`, `A-2` under canonical `A`) MAY map to
it; a different model (`B-1`) SHALL NOT. When no deployment of the requested
model can serve, the gateway SHALL return the existing exhausted error rather
than substituting another canonical model — even a stronger one.

#### Scenario: Variant upstream ID serves the canonical

WHEN canonical `A` maps a deployment whose upstream ID is `A-2`
THEN a request for `A` may be served by that deployment.

#### Scenario: Exhausted model never substitutes

WHEN every deployment of the requested model is ineligible
THEN the gateway returns the exhausted error and does not route the request to
a different canonical model.

### Requirement: Configured catch-all route

The gateway SHALL optionally route unknown model IDs through a single model
named `*`, passing the requested ID unchanged to its deployments. The catch-all
SHALL have no aliases or allowed keys, SHALL require `upstream_model = "*"`,
and SHALL NOT appear in model discovery. Other models SHALL NOT use that upstream
model value. Only the catch-all MAY declare `exclude`, containing nonempty exact
IDs or prefixes with a single trailing `*`, matched ASCII case-insensitively.
The literal request model `*` SHALL always be unknown. Normal routing limits,
priority, retries, and stream-commit rules SHALL remain unchanged.

#### Scenario: Unknown ID routed to catch-all

WHEN an ID has no configured name or alias and is not excluded
AND a catch-all is configured
THEN the catch-all deployments receive the original requested ID.

#### Scenario: Excluded ID retains unknown-model error

WHEN an unknown ID matches an exact exclusion or case-insensitive prefix exclusion
THEN the gateway returns the existing unknown-model error without an upstream call.

#### Scenario: Known model never uses catch-all

WHEN a configured name or alias is requested
THEN only that model's deployments are eligible, even if they are exhausted
AND its existing allowed-key restrictions remain enforced.

### Requirement: Eligibility before ranking

The gateway SHALL skip a deployment that cannot serve right now — in cooldown,
reporting zero remaining limit, at its concurrency cap, past its configured
`expires_at`, or parked because the provider does not provision it — before
applying priority ordering. An expired or not-provisioned deployment SHALL
NOT consume an attempt or quota and SHALL NOT mark a transient upstream
failure; it stays ineligible until the config changes.

#### Scenario: Cooling-down deployment skipped

WHEN the top-priority deployment is in cooldown
THEN the request goes to the next eligible deployment regardless of priority.

#### Scenario: Expired promo deployment skipped

WHEN a deployment's `expires_at` timestamp is in the past
THEN it is skipped as permanently ineligible, even if it has quota left
AND its last update to config or a reload is what restores it.

#### Scenario: Expired deployment reports its end

WHEN an operator reads deployment state
THEN each deployment exposes its `expires_at` and whether it has expired.

#### Scenario: Unprovisioned deployment skipped

WHEN a deployment is parked because the provider reported no API allowance
THEN it is skipped as permanently ineligible until the next config reload,
while sibling deployments on the same credential stay eligible.

#### Scenario: Request cannot be converted for the first deployment

WHEN a request uses a feature the selected deployment's adapter cannot convert
THEN the gateway skips that deployment without consuming an attempt or quota
or recording an upstream failure, and considers the next deployment.

#### Scenario: Conversion rejected even by rate-limited deployments

WHEN no eligible-format deployment can convert the request, including those
temporarily rate-limited
THEN the gateway returns an invalid-request error rather than a rate-limit error.

### Requirement: Quota window tracking

The gateway SHALL model each periodic quota as a window bound to a
(credential, quota-family) pair, tracking its reset time — provider-reported
resets are authoritative even beyond the configured cooldown cap — the
remaining amount, and whether the current window has started but seen no
usage. An exhausted window SHALL park its deployments until the reset rather
than dropping or hammering them. A reset window with no recorded usage SHALL
be treated as full allowance. Daily caps, per-minute limits, and expiry dates
on free deployments SHALL be tracked the same way, so a temporarily exhausted
leg is never pruned for today's exhaustion alone.

#### Scenario: Parked leg revives at reset

WHEN a deployment's daily-quota window resets at UTC midnight
THEN it is skipped until the reset and becomes eligible again afterwards.

#### Scenario: Fresh window counts as full

WHEN a credential's weekly window has reset but recorded no usage yet
THEN the gateway ranks it as holding its full allowance rather than assuming
it is spent.

### Requirement: Quota-expiry-first priority

Among eligible deployments the gateway SHALL order by:

1. Quota that expires: sooner expiry first, since unused allowance is lost.
2. Refill period: shorter refill cycles before longer ones.
3. Free before paid when the above are equal.

#### Scenario: Expiring quota preferred

WHEN deployment A's quota expires in 1 hour and deployment B's resets daily
THEN A is tried first.

A deployment MAY declare `priority_boost` (default `0`) with
`priority_boost_until` (an RFC 3339 timestamp) to rank by a quota deadline
the provider cannot report: while the timestamp is in the future the
deployment competes at its boost value instead of `priority`; once it passes
the deployment reverts to `priority` and stays eligible. `priority_boost`
without `priority_boost_until` SHALL be rejected at parse.

#### Scenario: Deadline boost promotes then reverts

WHEN a priority-1 deployment boosts to -1 until a future timestamp
THEN it outranks a priority-0 deployment until that time
AND reverts to the configured order afterward.

### Requirement: Remaining-amount scoring

The gateway SHALL score expiring-quota deployments by remaining amount
relative to time until expiry, not expiry time alone.

#### Scenario: Nearly empty quota deprioritized

WHEN a deployment expires soon but has almost no quota left
THEN a fuller deployment with a slightly later expiry may outrank it.

### Requirement: Concentrate on the best route

The gateway SHALL send traffic to the top-ranked deployment until it nears its
limits, and only then spill to the next. Load spreading is a safety valve for
avoiding rate-limit and latency spikes, not a goal in itself.

#### Scenario: Spillover near the cap

WHEN the top deployment approaches its request or concurrency limit
THEN overflow requests go to the next-ranked eligible deployment.

### Requirement: Cache-aware matching

When ranking is otherwise close, the gateway SHALL prefer the deployment most
likely to hold a warm prefix cache for the request — matching requests from
the same session to the same deployment — so cache hit rates stay high across
many concurrent sessions.

#### Scenario: Session affinity for cache hits

WHEN a session's next request shares a long prompt prefix with its previous
one
THEN it is routed to the same deployment when it remains eligible, rather than
spreading evenly and cold-starting the cache.

### Requirement: Quality floor

The gateway SHALL deprioritize a deployment whose observed latency or error
rate degrades, even for the same model.

#### Scenario: Degraded deployment ranked lower

WHEN two deployments serve the same model and one's error rate rises
THEN the healthier deployment is preferred until the degraded one recovers.

### Requirement: Paid deployments last

The gateway SHALL use paid deployments only when free and subscription quota
is exhausted, and only within a configured cost cap.

#### Scenario: Paid fallback at the end

WHEN every free deployment is ineligible and a paid deployment remains
THEN the request goes to the paid deployment only if the cost cap allows.

### Requirement: Revenue-aware capacity

For deployments that also serve CheaperInference, the gateway SHALL preserve
quota for models that sell well instead of spending it on low-value traffic.

#### Scenario: High-selling model keeps quota

WHEN a deployment's quota is shared between a high-selling model and a
low-value one
THEN the low-value model is deprioritized first as quota runs low.

### Requirement: Selection phases are explicit

Deployment selection SHALL follow the `Select` → `Admit` → `Build` → `Gate` →
`Send` phases of `machine::State`; a cooldown that appears during `Build` SHALL
send the request back to `Select` without consuming an attempt.

#### Scenario: Cooldown set during build

WHEN a deployment enters cooldown while its request is being built
THEN `Gate` skips it without an attempt and the next candidate is considered.

### Requirement: Live Devin catalog

The gateway SHALL refresh `devin models` at most once per 60 seconds and cache
the last successful result. A failed refresh SHALL NOT discard a loaded catalog.
While no catalog has loaded, configured names SHALL be listed unchanged. Chat,
messages, and responses requests SHALL NOT wait on a refresh.

`GET /v1/models` SHALL omit a configured name or alias only when every
deployment of that model is `devin_acp` and none of their `upstream_model`
values are in a successfully loaded catalog. It SHALL NOT add an id that the
config does not name. A new public name appears only after import or a config
reload.

On the request path the gateway SHALL skip a `devin_acp` deployment whose
`upstream_model` is absent from a loaded catalog the same way it skips a cooled
deployment, so another deployment of the same model can run. It SHALL NOT
substitute a different model. If none remain, it SHALL return the existing
exhausted error.

#### Scenario: Stale Devin id omitted after a successful load

WHEN the catalog loads successfully and does not contain the only upstream id
of a `devin_acp` model
THEN that model and its aliases are absent from `GET /v1/models`
AND an id present only in the catalog is not listed.

#### Scenario: Failed load keeps configured names

WHEN the catalog has never loaded and a refresh fails
THEN `GET /v1/models` still lists every configured name and alias.

#### Scenario: Stale deployment skipped

WHEN a loaded catalog lacks a `devin_acp` deployment's upstream id
AND the same model has another deployment whose upstream id is in the catalog
THEN the request skips the stale deployment without invoking it
AND the other deployment can serve.

#### Scenario: All Devin deployments stale

WHEN every deployment of the requested model is a stale `devin_acp` deployment
THEN the gateway returns the existing exhausted error
AND does not route the request to a different model.

### Requirement: Adaptive free-leg advertising

A client key whose `key_policies` entry sets `free_only = true` SHALL be
advertised a canonical model only while at least one usable free
(non-billed) deployment exists for it, and SHALL never have a request routed
to a billed leg — equivalent to `x-llmux-free-only: true` on every request.
A model whose every leg is billed, or whose free legs all sit on disabled
credentials, SHALL NOT appear in that key's `/v1/models` list, while a model
shared between free and billed legs stays listed as long as a usable free
leg remains. "Free" covers any leg with zero per-request cost — promotional
free tiers and subscription/plan-covered legs alike — and advertising SHALL
be recomputed automatically whenever the deployment set, billing
classification, or credential-disabled state changes, never maintained by
hand. Keys without the flag see every configured name their policy allows.

#### Scenario: Billed-only model hidden from a free-only key

WHEN every deployment of a model is billed
THEN a free-only key's `/v1/models` omits it and its requests fail without
an upstream call.

#### Scenario: One free leg keeps the model listed

WHEN a model has one free deployment alongside billed ones
THEN it stays advertised to a free-only key, and requests use the free leg
first.

#### Scenario: Expired plan leg stops counting

WHEN a model's only free deployment draws on a disabled credential
THEN the model drops out of a free-only key's `/v1/models` list until a
usable free leg returns.
