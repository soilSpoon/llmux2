# OmniRoute importer

```sh
cargo run --locked -p llmux-import -- --input export.json --output llmux.toml
# Optional: --secrets-dir /absolute/private/directory
cargo run --locked -p llmux-server -- llmux.toml
```

Use OmniRoute's operator-only `GET /api/settings/export-json` export
(`_meta.version = "omniroute-v3-legacy-export"`). Treat the input as sensitive:
it contains plaintext credentials. Unknown fields are tolerated. Unversioned
legacy JSON is accepted with a warning, including combos stored as `data` JSON.

Raw SQLite table dumps are also auto-detected by the `provider_connections`
root; use the same CLI command without converting the file first. Supported
tables are `provider_connections`, `provider_nodes`, `combos`, and optional
`api_keys`. Snake_case columns and boolean or 0/1 `is_active` values are
normalized into the same import path. Combo `data` may be an object or JSON
string; steps retain their camelCase fields such as `connectionId`.
JSON columns (`custom_headers_json`, `provider_specific_data`, and
`rate_limit_overrides_json`) accept objects, encoded objects, or null;
malformed values fail without printing their contents. Connections and steps
marked `usable: false` are skipped with warnings in either input format.
Model aliases from `key_value.modelAliases` (or `key_value` table rows) and
enabled `model_combo_mappings` are imported. Other namespaces and overrides are
ignored with one summary warning. Provider-specific base URL overrides are
applied before node/catalog defaults. No table-dump contents are required as
repository fixtures.

Redacted credential placeholders with printable non-whitespace ASCII are
treated as opaque credentials and written only to secret files, never TOML or
logs. This permits offline validation, not working upstream authentication;
replace placeholders securely before use.

The output directory must already exist. The default secret directory is
`secrets/` beside the output. On Unix, the directory is secured to 0700 and each
secret is exclusively created at 0600; existing output/secret files are never
overwritten. TOML contains absolute `file:` references, never credential values.
Move the secrets and update references together when relocating the config.
On failure, newly created secret files may remain; review/remove them before
retrying. Non-Unix secure file creation is not supported.

Active API-key connections become credentials. Each nonempty combo becomes a
canonical model; connection priorities sort ascending within each step. Pinned
connections and allowed-connection lists restrict selection. Nested combos expand
inline (cycle/depth/deployment limits and a 1024-step work budget per top-level
combo, including references producing no deployments); nested routing policy is not preserved.
Round-robin combos use equal deployment priority; other unsupported strategies
are approximated as sequential priority. Warnings and an import summary go to
stderr. Model names may contain spaces or punctuation in llmux; generated IDs
are sanitized and deduplicated. If there are no client keys, set
`LLMUX_CLIENT_KEY` before starting the server.

Built-ins: openai, anthropic, openrouter, groq, deepseek (`ds` step alias),
mistral, xai, together, fireworks, cerebras, gemini, morph, aihorde, kilo, kilocode.
The URL/protocol table comes from
OmniRoute `open-sse/config/providers/registry/<provider>/index.ts`.
Custom `openai-compatible` chat and `anthropic-compatible` nodes use `baseUrl`;
custom prefix aliases are resolved. llmux appends `/chat/completions` or
`/messages`, so built-in URLs omit that suffix, retaining `/v1` where present.
For custom nodes, routing prefixes/internal node IDs are stripped from the
upstream model; model vendor segments such as `openai/` are preserved. Repeated
node routing segments are stripped, matching OmniRoute's resolver.
Connection `providerSpecificData.baseUrl` wins over the node/catalog URL;
chat/message endpoint suffixes are removed because llmux appends them.
Loopback, localhost (including subdomains), unspecified addresses, and mapped
IPv4 loopback addresses are skipped with a warning before writing credentials.
This does not test DNS resolution or network reachability. Morph is imported
only at its official `https://api.morphllm.com/v1` endpoint.
Gemini uses Google's documented OpenAI-compatible endpoint
`https://generativelanguage.googleapis.com/v1beta/openai`, not OmniRoute's native
Gemini protocol endpoint. See https://ai.google.dev/gemini-api/docs/openai.
Gemini connection overrides at Google's native `/v1beta` root are normalized
to `/v1beta/openai`; overrides at other roots are skipped with a warning.

AI Horde (`aihorde`) uses the official OpenAI-compatible
`https://oai.aihorde.net/v1` endpoint, not its native asynchronous API. Active
`no-auth`/`none` Horde connections use deployment `auth = "none"` with
`provider = "openai_compat"` and no credential or upstream secret file;
anonymous use may be restricted under load. API-key connections keep
their own key; a missing API key is not silently replaced. Vendor/model IDs
are preserved. Kilo (`kilo`, also OmniRoute's `kilocode` provider ID) uses
`https://ai-gateway.kilo.ai/api/v1` with an API key or the same explicit anonymous
auth mechanism. Anonymous requests send no Authorization header; Kilo's anonymous
limits apply per IP. Keyed Horde/Kilo connections with missing keys are skipped,
never downgraded to anonymous. Kilo custom nodes identified by prefix use the same
rules and must resolve to the official gateway URL. Native Horde `/api/v2` roots
are normalized to its official chat facade; other Horde roots are rejected.

Skip precedence is: unusable/inactive, replacement policy, auth/key validation,
then provider/URL validation (including loopback). Without explicit Workers AI
opt-in, Cloudflare direct/playground connections remain skipped with an official replacement hint;
DuckDuckGo/DeepAI bridges are dropped without an official replacement; HMS is
skipped because custom headers are unsupported. Hints identify custom nodes
by their provider prefix and contain only fixed documentation URLs, never
connection-supplied URLs. Anonymous connections still pass provider/URL validation.
When an anonymous-only import has no client keys, loader validation uses a
unique temporary public marker file, safe for concurrent imports sharing a
secrets directory, which is removed on success or error; generated config
still requires `LLMUX_CLIENT_KEY` and retains no upstream credentials.

### Workers AI, free-only keyed Kilo, and official OpenRouter replacements

```sh
cargo run --locked -p llmux-import -- \
  --input export.json --output llmux.toml \
  --cloudflare-account-id-file /private/cloudflare-account-id \
  --cloudflare-token-file /private/cloudflare-workers-ai.token \
  --kilo-free-models /private/kilo-models.json \
  --kilo-key-file /private/kilo.key \
  --openrouter-replacements
```

The public API retains `import(...)` with safe defaults and adds
`import_with_options(..., &ImportOptions { ... })` for these flags. Both
Cloudflare paths are required together. The account ID file is read and trimmed;
it must contain 32 hexadecimal characters. llmux-config does not interpolate
file references inside URLs, so only that non-secret account ID is embedded in
`https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1`.
The token file is **never opened or copied by the importer**; the output contains
its absolute `file:` reference. The same applies to the optional external Kilo
key. The runtime must be able to read both files. Import-time validation uses
the real loader with a temporary public stand-in for external credentials;
existence, permissions, and contents of these runtime-owned files are deliberately
not validated. All stand-ins are removed, including on errors. Source-export
credentials continue to be validated and written as before.

Workers AI opt-in replaces `cloudflare-direct`/`cfpd` and
`cloudflare-playground`/`cfp` (including custom nodes with those prefixes) with
the official endpoint, never the bridge URL. Inactive/unusable/OAuth connections
remain excluded. Other providers' loopback checks are unchanged. Models use a
small source-linked allowlist in `src/policy.rs`, emitting exact `@cf/...` IDs.
Unknown, deprecated, and explicitly paid-plan-only models are warned and skipped;
in particular `@cf/meta/llama-3.2-1b-instruct` is never emitted. Catalog and billing
eligibility can change; review this allowlist before deployment.

**There is no Workers AI neuron or account-wide spending budget.** Core supports
`rpm`, `rpd`, `tpm`, `tpd`, and concurrency limits, but request/token counts cannot
enforce Cloudflare's model-dependent free neuron allowance. Imported Workers AI
deployments use conservative `quota.rpm = 1`. This is **per deployment**, not per
account: combos, direct names, and failover can multiply usage. It does not
guarantee zero charges or stop daily allowance exhaustion. Use a Workers Free
account/no paid overage at Cloudflare and monitor its account-wide allowance.
See https://developers.cloudflare.com/workers-ai/platform/pricing/.

Keyed Kilo routes now fail closed unless `--kilo-free-models` supplies a saved
Kilo `/models` object (`{"data":[...]}`) or an array of model objects. Each row
needs `id` and `pricing.prompt` **and** `pricing.completion`, both exactly zero.
Use integer JSON `0` or decimal zero strings such as `"0.000"`; floating-point
numbers/exponent notation are conservatively rejected to avoid underflow or
rounding positive prices to zero. A bare array of names, `:free` suffix, or
`isFree` flag is not pricing evidence. Conflicting duplicate rows fail closed.
An empty catalog permits no keyed models; missing/malformed pricing excludes the
model. Refresh the snapshot before deployment; this is static policy, not live
price enforcement. Without a snapshot, keyed Kilo connections are skipped before
writing secrets. Anonymous Kilo behavior and authentication are unchanged.
`--kilo-key-file` adds keyed, official-endpoint free-only fallback connections,
after existing connections, without replacing anonymous authentication.

`--openrouter-replacements` explicitly enables the reviewed source-linked table
in `src/replacements.rs`; it never selects OpenRouter or xAI. It emits the listed
`openrouter/...` names only when the matching official endpoint is already
connected (including Mistral custom nodes such as `vibe`) or Workers AI is opted
in. This mode reserves the `openrouter/` namespace: available source combo/alias/
mapping routes for those names are replaced, not reused with unverified fallback
legs. Unavailable winning mappings still fail closed; all other namespaces keep
their existing precedence. Aliases targeting those OpenRouter IDs use the same
replacement path. Unknown IDs, new-key/paid-only
providers, unverified transports (including Gemma 31B via Gemini Interactions),
and unavailable targets remain unrouted with warnings. Source OpenRouter
connections are skipped in this mode; there is no OpenRouter fallback. Pinned
OpenRouter connection restrictions are not redirected to unrelated connections.
Reviewed `:batch` IDs are synchronous same-model alternatives, with a warning:
batch scheduling and discounts are not preserved. Existing connected Anthropic,
Gemini, and Mistral keys may still be billable; this flag is not a promise of
free access on those providers. Without it, existing OpenRouter behavior remains
unchanged.

### Public model aliases

The importer uses llmux's existing model `aliases` field: aliases share their
canonical model's deployments without duplicating routing state. Combo names win
collisions; enabled mappings take precedence over legacy `modelAliases`, ordered
by descending priority then ascending creation time. Mapping targets use combo
IDs, not display labels. Missing/skipped targets are warned and never redirected
to another provider. Exact legacy aliases may target a combo (also `combo/name`),
a provider/model string, or a `{ "provider": "...", "model": "..." }` pair.
Direct targets expand through the same provider/connection checks as combo legs;
they create a canonical provider/model entry when needed. Alias chains are
flattened with cycle detection and a 128-hop limit.

### Direct provider/model IDs

Valid, usable model legs in retained combos and resolved direct alias targets
also supply direct canonical model entries. For each observed provider/model
pair, the importer emits the registry ID and its supported routing alias
(`ds`/`deepseek`, `kc`/`kilocode`), or the custom node's ID and nonempty,
unambiguous prefix. Registry routing aliases are not reassigned to another provider.
Observed direct strings with redundant custom routing segments remain usable;
only matching routing segments are stripped upstream, not vendor segments.
Each direct model uses **all eligible connections of its provider**, in ascending
connection priority, even when its source combo leg was pinned or restricted.
Anonymous connections use the same credential-free deployment mechanism.
Combo and alias names (including unavailable winning mappings) reserve their
names; direct collisions are skipped with deterministic warnings. Direct entries
never participate in real combo-reference lookup or consume its expansion budget.
Codex is excluded, and existing connection/provider exclusion rules still apply.

No model catalog is invented or fetched. Node `models_path` values are discovery
paths, not model IDs; excluded-model lists are not positive catalogs. Provider
metadata in the inspected dump contains no positive model lists. Separate cached
`key_value` custom/synced catalogs are not used by this importer: this version
derives model IDs only from combo legs and resolved alias targets. A live
`/v1/models` response or traffic log is measurement input only.

Wildcard mappings are evaluated for known exact alias/mapping and combo-derived direct names; the
importer does not invent an unbounded model catalog. Wildcard alias names,
automatic catalog aliases, provider-scoped aliases, and settings-based aliases
are not imported. Unsupported or malformed entries produce warnings.

Not imported: OAuth/token/cookie auth, encrypted credentials, unsupported
providers/protocols (including custom Responses/custom-JSON nodes), custom chat
paths/headers, wildcard models, per-key policies, shared
account quotas, global priority, runtime state, and combo config/transforms.
Only unrestricted active gateway keys are imported. Keys with model/connection/
combo/endpoint/scope restrictions, schedules, expiry/revocation, bans, rate/session/
spending limits or other recognized access-limiting policies are skipped, never
promoted to unrestricted llmux access. Provider-specific model transformations (e.g. Fireworks model-prefix
rewrites) are not applied: combo models must contain the actual upstream ID.
The importer validates generated URLs/config with llmux-config before writing;
it does not contact upstream providers or validate that credentials work.
