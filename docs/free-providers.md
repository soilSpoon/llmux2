# Official free API providers

Research snapshot: **2026-11-20**, against llmux2 `eb3b617`; the temporary
offers in the first section were added **2026-11-23** and are configured in
[`examples/llmux.free-providers.example.toml`](../examples/llmux.free-providers.example.toml),
a separate sample because it needs four extra provider keys the quickstart does
not. Official pages were
retrieved with `curl -L -sS`; quotations below are the text returned, not remembered
quotas. Free offers and catalogs change. A documented API is not a promise of
capacity, production suitability, permission to resell, or identical models to an
OmniRoute bridge. **Unverified** means do not budget capacity or enable that offer
on the strength of this document.

The compatibility/config sections now include explicit keyless deployment support.
AI Horde's current official OpenAPI schema was rechecked for anonymous auth.

Only official API keys or explicitly documented anonymous access qualify. Do not
use playground internals, TLS impersonation, MITM, or consumer OAuth tokens in a
third-party client. Browser login to obtain an official API key is different from
replaying a consumer product's OAuth credentials. Codex and Morph are outside
this research; Gemini credential mapping is being handled separately. Factory is
excluded. No accounts were created and no paid inference was performed.

## Temporary free offers (promotions that appear and expire)

Promotions like XPLabs' 48-hour free windows recur: a provider lists a model at
100% off for a short time, then it reverts to paid or disappears. Track them as
deployments with `expires_at`, not as a separate list, so the same routing,
limits and stats machinery covers them.

- **Configure with an end time.** `expires_at = <RFC 3339 timestamp>` on
  `[[models.deployments]]` makes the deployment permanently ineligible once the
  promo ends — skipped like a disabled deployment, without consuming an attempt
  or recording a failure — until config changes via `/admin/reload` or restart.
  Use the provider's stated end time; when a promo lists none, pick a short
  conservative expiry (e.g. a few days) and let the tracker extend it.
- **Watch via `/admin/deployments`.** Each deployment reports `expires_at`
  and `expired`, so a tracking job reads live state instead of parsing
  `llmux.toml`. An external tracker (scheduled check of provider
  pricing/model pages, or a human filing a change) adds the deployment, sets or
  extends `expires_at`, and `POST /admin/reload`s — no restart, no downtime.
- **Expiry is not the only signal.** Promos can also end by rate-limiting
  harder or withdrawing the model; the existing cooldown and quota
  observation path handles those and fails over, so `expires_at` only needs to
  cover the announced end.
- **Priority:** give promo deployments low `priority` (tried last) unless the
  goal is to burn the free window first — the quota-expiry-first ordering
  already prefers allowances that lapse sooner.

### Offers configured on 2026-11-23

These four providers — seven deployments — ship as `[[models.deployments]]` in
[`examples/llmux.free-providers.example.toml`](../examples/llmux.free-providers.example.toml),
not in the quickstart example: the loader resolves every credential at startup, so
mixing them into `examples/llmux.example.toml` would make the llama-70b quickstart
demand all four extra keys. Run that file on its own or copy the blocks you want.
They are **test and failover capacity, not capacity you can budget**: every one of
them is a promotion, a free tier, or a third-party aggregator that can change
without notice. Each upstream limit in the example is set below the documented
limit so a burst degrades into a gateway-visible failure instead of an upstream
ban.

| Deployment | Upstream | Free window | Expiry in config |
| --- | --- | --- | --- |
| `packet-deepseek-v4.1-flash` | `api.packet.ai/v1`, `nvidia/DeepSeek-V4.1-Flash-NVFP4` | $0 per token until 2026-12-16, then paid | `expires_at = 2026-12-16T23:59:59Z` |
| `packet-minimax-m3.1-flash` | `api.packet.ai/v1`, `MiniMaxAI/MiniMax-M3.1-Flash` | same window | `expires_at = 2026-12-16T23:59:59Z` |
| `zen-hy4-flash` | `opencode.ai/inference/openai/v1`, `hy4-flash-free` | “for a limited time”, no date on the page | `expires_at = 2026-11-30T23:59:59Z` (the window this project was given, not an official date) |
| `zen-deepseek-v4.1-flash` | same endpoint, `deepseek-v4.1-flash-free` | “for a limited time” | none, because no end date is announced |
| `zen-big-pickle` | same endpoint, `big-pickle` | “for a limited time” | none |
| `nvidia-gemma-4-31b` | `integrate.api.nvidia.com/v1`, `google/gemma-4-31b-it` | Free Endpoint Available; no announced end | none |
| `freemodel-claude-fable-5` | `https://freemodel.in/v1`, `claude-fable-5` | 1M tokens/month on the Free plan, reset monthly | none; a recurring allowance is not an expiry |

Caveats that decide whether you can send real prompts to each one:

- **packet.ai Token Factory** requires an email and one SMS verification per
  account [P1]. The upstream model IDs above carry a vendor prefix that the
  marketing table does not print, so the exact strings `api.packet.ai` expects
  are **unverified**; a wrong ID surfaces as an upstream `404`/`400`, not as a
  config error. Its pricing page bills GPU rentals from a prepaid wallet, so that
  wallet is the account the free tokens land in; whether an empty wallet blocks
  Token Factory after the free window is **unverified**. Nothing on the page
  states a rate limit or a data-retention policy — both **unverified**.
- **OpenCode Zen** says “Free-tier chat models require OpenCode; use paid models
  or Go from other clients” [Z1]. An outside caller is therefore not guaranteed
  free capacity, and a keyless probe returned `429 FreeUsageLimitError`. Treat a
  `FreeUsageLimitError` here as rate limiting, not as an exhausted quota: the
  adapter maps `429` to `RateLimited` unless the body says quota, so the gateway
  cools the deployment down instead of marking it spent.
- **Big Pickle** is a “stealth model … free on OpenCode for a limited time” whose
  page says the team is using the window “to collect feedback and improve the
  model” [Z1]. The zero-retention sentence on that page belongs to DeepSeek V4.1
  Flash Free and Hy4 Flash Free, not to Big Pickle, so assume prompts sent to
  Big Pickle may be used for training.
- **freemodel.in** is a third-party aggregator in front of other providers'
  models. Its own terms are the only retention statement you get, and it resells
  upstream capacity: retention and resale permissions of the upstream model
  owner are **unverified**. The monthly cap is counted “both your prompt and the
  model's reply, combined”, and exhaustion returns `402` before the request
  reaches a model [F2].

### Monthly token allowances are not expressible yet

`quota` accepts `rpm`, `rpd`, `tpm`, `tpd` and `concurrent` only, and each is a
sliding window that resets within a minute or a day. A provider that grants
1,000,000 tokens per calendar month — freemodel.in's Free plan, with 60 RPM and
100,000 TPM on top [F2] — has no matching field. The example config therefore sets
a conservative `rpm` and relies on the upstream `402` for the monthly cap, which
`llmux2` maps to `quota_exhausted` and fails over. Adding a monthly limit is a
config and limits change, not something this document can work around.

## Import gaps → replacements

The dry-run counts are **group totals**, not per-provider counts: 43 bridge legs,
34 OAuth legs (including separately handled Codex), 7 no-auth legs, 1 custom-header
leg. These are not counts of legs that can be restored unchanged.

| Skipped connection | Official replacement | Free status and llmux2 decision |
| --- | --- | --- |
| `cloudflare-direct`, `cloudflare-playground` (43-leg bridge group) | Workers AI account API token, `/client/v4/accounts/ACCOUNT_ID/ai/v1` [CF1–2] | 10,000 neurons/day; `openai_compat`. Use a free-eligible model, not the playground catalog. In particular GLM-5.2 is explicitly paid-method-gated. |
| `deepai` (same group) | DeepAI's `https://api.deepai.org/api/<model>` with an `Api-Key` header [D1] | The `deepai` adapter converts text Chat Completions for `text-generator` and `summarization`. Neither authenticated generation nor free API use is verified. |
| `deepai-code` (same group) | Keyed DeepAI Code sidecar with Chat Completions | Use `openai_compat`, not `deepai`. The importer accepts the reviewed keyed sidecar at `http://127.0.0.1:8790/deepai/v1` for the exported node with the `deepai` prefix or built-in `deepai-code` connection. The listener must be reachable from llmux2. The live sidecar and pricing remain unverified. |
| `duckduckgo-web` / `ddgw` (same group) | No supported third-party Duck.ai inference API verified [D2] | Direct import remains unavailable. The existing opt-in importer replacement uses only reviewed same-model matches. |
| `antigravity` (34-leg OAuth group) | Google AI Studio/Gemini API key [G1–3] | Official free eligible models, project quotas, data-use caveats. `openai_compat`; do not migrate consumer OAuth. Five existing keys are not five independent project quotas. |
| `xai-oauth` / grok-cli (same group) | xAI API key at `https://api.x.ai/v1` [X1–2] | Official keyed access, but no current free credit grant verified. Do not count as a free replacement. Current quickstart demonstrates Responses; llmux2 has no native upstream `responses` kind. Chat compatibility/model eligibility must be checked separately. |
| `kilo`, `kilo-free` (7-leg no-auth group) | Same official gateway with Kilo API key; anonymous free access is also explicitly documented [K1–3] | Both keyed and explicit `auth = "none"` deployments work. Anonymous is 200 requests/hour/IP. |
| `aihorde` (same group) | Official OpenAI-compatible `https://oai.aihorde.net/v1`, Horde key or anonymous access [H1–4] | Omit Authorization: the service selects anonymous key `0000000000`. Works with `openai_compat`; native `https://aihorde.net/api/v2` is asynchronous and is not compatible. Queue/worker capacity, no fixed free token budget verified. |
| `hms` (1 custom-header leg) | Unknown operator/protocol; require the original operator's API/auth documentation | No connection named `hms` was present in the supplied connection list. Its bridge leg needed custom headers, which `[[models.deployments]] headers` can now carry. Do not infer legitimacy from a working bridge or import arbitrary headers. |

## Ranked provider shortlist

Rank favors verified recurring free access and useful compatibility, not benchmark
quality. Rows marked conditional/trial are **not** recommended as unattended free
production fallback. All compatible rows use `provider = "openai_compat"` with
Bearer auth unless stated otherwise; no additional provider kind is needed for
their Chat Completions subset. Model examples are from the linked official docs
or live catalog, not a guarantee that they remain free or available.

| Rank / provider | Base URL; auth | Verified free allowance; notable models | Eligibility, data/ToS caveats; as-is? |
| --- | --- | --- | --- |
| 1 Cloudflare Workers AI | `https://api.cloudflare.com/client/v4/accounts/ACCOUNT_ID/ai/v1`; account API token | 10,000 neurons/day [CF1]; `@cf/meta/llama-3.1-8b-instruct` [CF2] | Keep Workers Free to avoid paid overage; several frontier models require paid billing. Training, resale and multi-account clauses **unverified**. Yes, Chat. |
| 2 Kilo Gateway | `https://ai-gateway.kilo.ai/api/v1`; Kilo API key, or official anonymous | Anonymous 200 requests/hour/IP [K2]; `kilo-auto/free`, examples `minimax/minimax-m2.1:free`, `z-ai/glm-5:free` [K2–3]. Keyed numeric limit **unverified** | Free routes can log/train; NVIDIA routes trial-only, no confidential data [K3]. Both keyed and anonymous supported. Do not assume anonymous quota also applies to keyed requests. |
| 3 OpenRouter | `https://openrouter.ai/api/v1`; API key | 20 RPM, 50 RPD without qualifying credit purchase; 1,000 RPD after threshold [O1]. `openrouter/free` or catalog-listed `:free` variants [O2] | Separate training controls for free routes [O3]. Paid fallback models cost money. Additional accounts/keys do not increase limits [O1]. Resale permission **unverified**. Yes. |
| 4 Google AI Studio (already being mapped) | `https://generativelanguage.googleapis.com/v1beta/openai`; Gemini API key | Free tokens on eligible models [G1]; exact current RPM/TPM/RPD account-dependent, not verified. Compatibility example `gemini-4-hydrogen` is **not evidence of free eligibility** [G3] | Unpaid prompts/responses used for product/model improvement, regional exceptions in terms [G4]. Quotas per project [G2]; do not rotate keys to bypass them. Yes, Chat. |
| 5 AI Horde | `https://oai.aihorde.net/v1`; Horde key or public anonymous key | Free queue-based access [H1]; live example `koboldcpp/Artemis-31B-v1.2` [H3]; numerical quota **unverified** | Anonymous lowest priority, may be restricted at high load. Volunteer workers see requests; do not send secrets. Kudos cannot be bought/sold [H1]. Yes via official chat front, not native v2 API. |
| 6 SambaNova (evaluation only) | Console-issued base URL; historical `https://api.sambanova.ai/v1` **not reverified**; API key | Free Tier without payment method; free table `gpt-oss-120b` and `gemma-4-31B-it`: 20 RPM / 20 RPD / 200,000 TPD [S1] | All SambaCloud models preview, explicitly not for production [S1]. Training/resale/multi-account rules **unverified**; website terms are not API-service terms. Chat SDK yes [S2]; get current URL from console before use. |
| 7 Cohere (evaluation only) | `https://api.cohere.ai/compatibility/v1`; trial API key | 1,000 calls/month; Command A family 20 requests/min [C1]. Compatibility example `command-a-plus-05-2026` [C2] | Trial/evaluation, not a production serving entitlement. Data training, resale and multi-account clauses **unverified**. Yes, supported compatibility subset. |
| Devin SWE-2 | local `devin acp` CLI bridge, not an HTTP upstream; `provider = "devin_acp"` with `auth = "none"` | `swe-2-high` marked **Free** in the `devin models` catalog [DV1]; `swe-2-high-lightning` is paid ($1 / 1M input) | Needs the Devin CLI installed and signed in under `LLMUX_DEVIN_HOME`; it runs the owner's own Devin account, so do not treat it as a shared free pool. Chat via the bridge only. |
| Conditional: Groq | `https://api.groq.com/openai/v1`; API key [R2] | Current free-plan entitlement and numerical free limits **unverified** in retrieved rate-limit page (account Limits page required) [R1]. `openai/gpt-oss-20b`, `openai/gpt-oss-120b` [R2–3] | Do not copy historical 30 RPM/14,400 RPD into config. Organization-scoped limits [R1]; default no retention with exceptions [R4]. Resale/multi-account terms **unverified**. Chat compatible [R2]. |
| Trial only: Cerebras | `https://api.cerebras.ai/v1`; API key [B2] | $5 credits, verified payment method, expires 30 days; explicitly no permanently free tier [B1]. `gpt-oss-120b`: 5 RPM, 30K uncached TPM, 90K total TPM, 1M TPH/TPD | Not recurring free. `qwen-3.8-27b` also listed; do not infer its separate limits from GPT-OSS. Training/resale/multi-account terms **unverified**. Chat yes. |
| Trial only: Scaleway | Console endpoint; current base URL/auth details **unverified** in fetched quickstart [SC2] | New customers get 1,000,000 tokens, then paid [SC1]; `glm-5.3` listed | Not verified as recurring monthly. OpenAI-compatible per official product page; billing/retention/resale/multi-account details **unverified**. No ready-to-enable config until endpoint/key confirmed. |
| Conditional: NVIDIA NIM / build.nvidia.com | `https://integrate.api.nvidia.com/v1`; Developer API key [N1, N3] | `google/gemma-4-31b-it` page says “Start building with a free API endpoint” and **Free Endpoint Available** [N4]. Numerical limit **unverified**: the quickstart gives none, third parties report ~40 RPM [N1] | Trial use only; the Llama 3.1 8B page still says **Free Endpoint Deprecated** [N2], so per-model availability is not universal. Do not copy the old 1,000-credit figure into config. Terms **unverified**. Chat yes; configured with `rpm = 20` in the example. |
| Hold: Mistral La Plateforme / AI Studio | `https://api.mistral.ai/v1`; API key [M1] | Current Experiment/free allowance **unverified**. `mistral-small-latest` official example | Old tier URL returned 404. Current quickstart requires a plan with API access and points to adding a payment method; limits in Admin [M1–2]. Training, resale and multi-account rules **unverified**. Chat yes, but not established free. |
| Exclude: Hugging Face Inference Providers | `https://router.huggingface.co/v1`; HF token | Current free-user credits **None**; paid PRO $2/month [F1]. Example `deepseek-ai/DeepSeek-V3-0324` | Purchasing credits/PRO is not free. Underlying provider/model terms apply; training/resale/multi-account details **unverified**. Chat yes technically. |
| Exclude: Chutes | Current API base/auth/model catalog **unverified** | “We do not offer a free tier at this time.” [CH1] | Pay-as-you-go, not a free replacement. Compatibility, data and resale terms **unverified** in fetched pricing page. |
| Exclude: GitHub Models | No active endpoint/auth | Official docs say fully retired July 30, 2026 [GH1]; no current free models/limits | Do not configure legacy `models.github.ai/inference` or PAT examples. No. |
| Hold: LLM7 | `https://api.llm7.io/v1`; dashboard API token; `default`, `fast`, `pro` selectors [L1] | No current free allowance or limits verified. Homepage advertises lower prices, not free access | First-party docs establish keyed OpenAI client support, not underlying provider authorization. Training/resale/multi-account terms and provenance **unverified**. Technically Chat yes; do not recommend until those are established. |
| Temporary: packet.ai Token Factory | `https://api.packet.ai/v1`; account API key after email plus SMS verification [P1] | Input and output tokens `$0` for every account until 2026-12-16, then paid [P1]. `DeepSeek-V4.1-Flash-NVFP4` and `MiniMax-M3.1-Flash`, 131K ctx [P1]. Rate limit **unverified** | Promo, not recurring. Endpoint and model IDs rest on the landing page only: keyless `/v1/models` returned `403`, docs behind a challenge [P1]. Retention policy unstated. Chat with streaming, JSON mode and tool calling per the page. |
| Conditional: OpenCode Zen free pool | `https://opencode.ai/inference/openai/v1`; Zen API key [Z1] | `*-free` IDs priced 0 in the live catalog (`deepseek-v4.1-flash-free`, `hy4-flash-free`, `big-pickle`, others); all “for a limited time”, no dates [Z1] | “Free-tier chat models require OpenCode”; a keyless outside request got `429 FreeUsageLimitError` [Z1]. Zero retention is stated for DeepSeek V4.1 Flash Free and Hy4 Flash Free only; Big Pickle may be trained on. |
| Conditional: freemodel.in Free plan | `https://freemodel.in/v1`; account key [F2] | 1M tokens/month, prompt and reply combined, 60 RPM, 100K TPM; `402` when spent [F2]. 29 models in the catalog, including `claude-fable-5` | Third-party aggregator reselling other providers' models; its retention terms are the only ones you get, so upstream training/resale terms are **unverified**. Not an expiry: the allowance resets monthly. Monthly token caps are not expressible in `quota`. |

Do not treat “unverified” ToS fields as permission. Before serving other users,
check the exact API service agreement, upstream model license, geographic/age
restrictions, training controls and resale restrictions for the chosen account.
Do not aggregate trial accounts, create accounts to bypass quotas, or publish
shared keys. A personal, authenticated gateway is the conservative starting point;
this review does not clear any provider for commercial resale.

## Official evidence: exact text and URLs

Quotes preserve returned wording (table rows are flattened to spaces). Where a
page only exposes account-specific limits, no number is invented.

- **CF1** [Workers AI pricing](https://developers.cloudflare.com/workers-ai/platform/pricing/):
  “Our free allocation allows anyone to use a total of 10,000 Neurons per day at no charge.”
  “All limits reset daily at 00:00 UTC.” Also: “Some models require a paid billing method.”
  The following list includes `@cf/zai-org/glm-5.2`; a bridge's GLM model is not a free Workers AI entitlement.
- **CF2** [OpenAI compatibility](https://developers.cloudflare.com/workers-ai/configuration/open-ai-compatibility/):
  “Workers AI provides OpenAI-compatible endpoints for text generation through Chat Completions”
  and “for text embedding models”.
  Example base: `https://api.cloudflare.com/client/v4/accounts/${env.CLOUDFLARE_ACCOUNT_ID}/ai/v1`;
  curl auth: `Authorization: Bearer {api_token}`; example model `@cf/meta/llama-3.1-8b-instruct`.
  “The Responses API is supported only by the @cf/openai/gpt-oss-120b and @cf/openai/gpt-oss-20b models.”
  “Responses requests must be non-streaming, only stream: false is supported.” Use Chat here.
- **K1** [Gateway](https://kilo.ai/docs/gateway):
  “The Kilo AI Gateway provides a unified, OpenAI-compatible API to access hundreds of AI models through a single endpoint at https://ai-gateway.kilo.ai/api/v1”.
- **K2** [Authentication](https://kilo.ai/docs/gateway/authentication):
  “API keys are JWT tokens tied to your Kilo account.”
  “The gateway allows unauthenticated access for free models only. Anonymous requests are identified by IP address and are subject to rate limiting (200 requests per hour per IP).”
  “Free models include models tagged with :free in their model ID, such as minimax/minimax-m2.1:free and z-ai/glm-5:free.”
- **K3** [Models and providers](https://kilo.ai/docs/gateway/models-and-providers):
  “Free with limited capability. No credits required.” (`kilo-auto/free`).
  “Auto Free may route your requests to providers that log prompts and outputs and use them to improve their services”.
  “For NVIDIA free endpoints (Super/Ultra/etc): Trial use only - do not submit personal or confidential data.”
- **H1** [Official AI Horde repository README](https://raw.githubusercontent.com/Haidra-Org/AI-Horde/main/README.md):
  “Anyone can use the AI Horde for free, even [anonymously]”.
  “The Horde has an OpenAI-compatible endpoint at `https://oai.aihorde.net/v1`.”
  “Send a Horde API key as the bearer token, or use the anonymous key `0000000000`.”
  “Anonymous use may be restricted when the Horde is under high load.”
  “Kudos cannot be bought or sold. Trading them is against the [Terms of Service]”.
- **H2** [Chat front reference](https://haidra-org.github.io/AI-Horde/reference/chat_front/):
  “The chat front exposes OpenAI-compatible chat completions and model listings through the Horde's native /v2/generate/chat routes.”
  This disproves the assumption that Horde necessarily needs a new llmux2 adapter.
- **H3** [Official live model catalog](https://oai.aihorde.net/v1/models): returned
  `"id":"koboldcpp/Artemis-31B-v1.2"` and other worker-dependent IDs. This is a
  catalog check, not a successful generation/latency test.
- **H4** [Official chat front OpenAPI schema](https://oai.aihorde.net/openapi.json),
  discovered from the Swagger UI at [the official root](https://oai.aihorde.net):
  `AIHordeApiKey` description: “An AI Horde API key. Omit it to use the anonymous key `0000000000`.”
  Therefore true keyless access is supported, without a stored anonymous key.
  Importer mapping of anonymous connections is handled separately in PR #28.
- **O1** [OpenRouter limits](https://openrouter.ai/docs/api-reference/limits.md):
  “Making additional accounts or API keys will not affect your rate limits, as we govern capacity globally.”
  Free usage table headings: “Credits purchased (all time) | Requests per minute | Requests per day”;
  rows: “Less than 10 | 20 | 50” and “At least 10 | 20 | 1000”.
  The page also describes a rounding/top-up concession one credit below the
  threshold; use `GET /api/v1/key` for the actual ceiling, not that concession as a budget guarantee.
- **O2** [Free variants](https://openrouter.ai/docs/guides/routing/model-variants/free.md):
  “Append `:free` to the ID of a model that has a free entry.”
  “To keep a request free, list only `:free` models in `models`” or use
  `openrouter/free`, “which falls back only to other free models.”
  The example variant is `meta-llama/llama-3.2-3b-instruct:free`.
- **O3** [Provider logging](https://openrouter.ai/docs/guides/privacy/logging):
  “There are separate settings for paid and free models.”
  “If you opt out of training in your account settings, OpenRouter will not route to providers that train.”
- **G1** [Gemini pricing](https://ai.google.dev/gemini-api/docs/pricing): free plan lists
  “Limited access to certain models”, “Free input & output tokens”, and
  “Content used to improve our products”. Do not generalize this to every Gemini model.
- **G2** [Gemini rate limits](https://ai.google.dev/gemini-api/docs/rate-limits):
  “Rate limits are applied per project, not per API key.”
  “View your active rate limits in AI Studio”. Exact account quotas were not accessible here.
- **G3** [Gemini OpenAI compatibility](https://ai.google.dev/gemini-api/docs/openai):
  `base_url="https://generativelanguage.googleapis.com/v1beta/openai/"`,
  `api_key="GEMINI_API_KEY"`, example `model="gemini-4-hydrogen"`.
- **G4** [Gemini API terms](https://ai.google.dev/gemini-api/terms):
  “When you use Unpaid Services, including, for example, Google AI Studio and the unpaid quota on Gemini API, Google uses the content you submit to the Services and any generated responses to provide, improve, and develop Google products and services and machine learning technologies”.
  Read the entire terms for regional exceptions and sensitive-data restrictions.
- **S1** [SambaNova rate limits](https://docs.sambanova.ai/docs/en/models/rate-limits):
  “Free Tier : Applied when there is no payment method linked with your account”.
  Free Tier table headings are “Developer Model ID RPM RPD TPD”; rows
  “OpenAI gpt-oss-120b 20 20 200000” and “Google gemma-4-31B-it 20 20 200000”.
  “All SambaCloud models are Preview models: they are intended for evaluation purposes and developer experimentation only, and should not be used in production environments.”
- **S2** [SambaNova quickstart](https://docs.sambanova.ai/docs/en/get-started/quickstart):
  OpenAI client example uses `base_url="your-sambanova-base-url"`,
  `api_key="your-sambanova-api-key"`; curl uses `Authorization: Bearer $API_KEY`.
  Current concrete public base URL was not established by that placeholder.
- **C1** [Cohere rate limits](https://docs.cohere.com/docs/rate-limits):
  “Cohere offers two kinds of API keys: evaluation keys (free but limited in usage), and production keys (paid and much less limited in usage).”
  “Trial keys (and prod keys on newer Chat model variants) are limited to 1,000 API calls a month.”
  Table: “Command A+ 20 req / min Contact sales@cohere.com”.
- **C2** [Cohere compatibility API](https://docs.cohere.com/docs/compatibility-api):
  `base_url="https://api.cohere.ai/compatibility/v1"`, model `command-a-plus-05-2026`.
- **R1** [Groq rate limits](https://console.groq.com/docs/rate-limits):
  “Rate limits apply at the organization level, not individual users.”
  The retrieved body did not establish numerical free-plan limits. Do not mistake
  example response-header values for an account's entitlement.
- **R2** [Groq OpenAI compatibility](https://console.groq.com/docs/openai) and
  [overview](https://console.groq.com/docs/overview): official client base
  `https://api.groq.com/openai/v1`, env key `GROQ_API_KEY`, example `openai/gpt-oss-20b`.
- **R3** [Groq models](https://console.groq.com/docs/models): lists
  `openai/gpt-oss-120b` and `openai/gpt-oss-20b`; published developer/paid limits
  are not free-plan limits.
- **R4** [Groq data](https://console.groq.com/docs/your-data):
  “By default, Groq does not retain customer data for inference requests.”
  The same page gives feature-state and reliability/abuse-monitoring exceptions.
- **B1** [Cerebras rate limits](https://inference-docs.cerebras.ai/support/rate-limits):
  “New accounts receive $5 in free credits after adding a verified payment method. These credits expire 30 days after they’re granted and can be used across all public models.”
  “Cerebras doesn’t currently offer a no-cost tier that renews automatically or a per-model always-free allowance.”
  Free Trial row: “gpt-oss-120b 5 30K 90K 1M 1M” under RPM / Uncached TPM / Total TPM / TPH / TPD.
- **B2** [Cerebras quickstart](https://inference-docs.cerebras.ai/quickstart):
  `https://api.cerebras.ai/v1/chat/completions`, `Authorization: Bearer ${CEREBRAS_API_KEY}`.
- **SC1** [Scaleway Generative APIs](https://www.scaleway.com/en/generative-apis/):
  “Every new customer gets 1,000,000 free tokens—start paying only from the 1,000,001st token.”
- **SC2** [Scaleway quickstart](https://www.scaleway.com/en/docs/generative-apis/quickstart/):
  console-driven model/endpoint instructions; no free quota renewal verified.
- **N1** [NVIDIA API quickstart](https://docs.api.nvidia.com/nim/docs/api-quickstart):
  “Follow the instructions below to set up an API Key and send a request to a model.”
  “If you already have an NVIDIA Developer API Key, skip to step 4.” No free quota verified there.
- **N2** [NVIDIA Llama model](https://build.nvidia.com/meta/llama-3_1-8b-instruct):
  “Model Availability Free Endpoint Deprecated Partner Endpoint Available Download Available”.
- **N3** [NVIDIA models](https://integrate.api.nvidia.com/v1/models): returned HTTP 200
  model catalog; listing is not a free-generation entitlement.
- **M1** [Mistral API key quickstart](https://docs.mistral.ai/getting-started/quickstarts/studio/activate-and-generate-api-key):
  Requires a Mistral account “on a plan with API access — see Set up billing to add a payment method.”
  curl uses `https://api.mistral.ai/v1/chat/completions`, Bearer key, `mistral-small-latest`.
- **M2** [Mistral usage and limits](https://docs.mistral.ai/admin/billing-usage/usage-limits):
  “Review API rate limits” → “Admin Panel › API › Limits”. No free numerical allowance verified.
  Older `https://docs.mistral.ai/deployment/ai-studio/tier` returned HTTP 404.
- **F1** [HF pricing](https://huggingface.co/docs/inference-providers/pricing):
  “Free users can use Inference Providers by purchasing credits, or by upgrading to PRO”.
  Table: “Free Users None - yes (credits purchase required)”. OpenAI example base
  `https://router.huggingface.co/v1` with `HF_TOKEN`.
- **CH1** [Chutes pricing](https://chutes.ai/pricing):
  “We do not offer a free tier at this time. Top up your account in the app to get started.”
- **GH1** [GitHub Models](https://docs.github.com/en/github-models/prototyping-with-ai-models):
  “As of July 30, 2026, GitHub Models has been fully retired. The playground, model catalog, inference API, and bring your own key (BYOK) are no longer available to any customer.”
- **L1** [LLM7 quickstart](https://docs.llm7.io/quickstart.md):
  “OpenAI SDK requires an API token from [dash.llm7.io]”.
  Example `base_url="https://api.llm7.io/v1"`, `model="default"` with comment
  `# or "fast", "pro", or a model ID from /v1/models`. Free limits unverified.
- **D1** [DeepAI docs](https://deepai.org/docs):
  “Pass your key in the api-key header and the model's inputs as form fields. The endpoint URL is https://api.deepai.org/api/<model>”.
  Keyless probes returned HTTP 401 for `text-generator` and `summarization`.
  That verifies endpoint recognition, not successful generation or free API use.
- **D2** [Duck.ai terms](https://duckduckgo.com/duckai/privacy-terms):
  Prohibit automated querying and circumventing safeguards. Status requests to
  both Duck.ai hosts returned a browser challenge, not a plain anonymous token.
- **X1** [xAI overview](https://docs.x.ai/docs/overview):
  `base_url="https://api.x.ai/v1"`, `model="grok-4.7"` in Responses example;
  navigation says “Purchase credits”.
- **X2** [xAI rate limits](https://docs.x.ai/docs/key-information/consumption-and-rate-limits):
  tiers are “based on total revenue received through prepaid credit purchases or successfully fulfilled invoices.”
  No current free-credit entitlement verified. Do not assume old promotions still apply.
- **P1** [packet.ai Token Factory](https://packet.ai/) (retrieved 2026-11-23):
  “Token Factory serves open-weight models behind an OpenAI-compatible API. Change
  the base URL, keep your code.” “Every token, input and output, is free until
  December 16, 2026.” “Set base_url to api.packet.ai/v1 and reuse your existing
  OpenAI client and prompts.” Model table: `DeepSeek-V4.1-Flash-NVFP4` and
  `MiniMax-M3.1-Flash`, both “131K ctx”, `$0` “Until Dec 16”. “After that you pay
  per token, with input and output metered separately.” Phone verification is
  required before the first request. No rate limit and no retention policy stated
  on the page. `https://api.packet.ai/v1/models` without a key returned `403
  {"error":{"message":"Request blocked by security policy"}}`; the docs links
  redirect to `dash.packet.ai/docs`, which returned `403` behind a challenge — so
  the model IDs and base URL rest on this page alone.
- **Z1** [OpenCode Zen](https://opencode.ai/docs/zen/) (retrieved 2026-11-23):
  “Free-tier chat models require OpenCode; use paid models or Go from other
  clients.” Free models section: “DeepSeek V4.1 Flash Free is free on OpenCode for
  a limited time. Its provider follows a zero-retention policy and does not use
  your data for model training.” “Hy4 flash Free is free on OpenCode for a limited
  time. Its provider follows a zero-retention policy…”. “Big Pickle is a stealth
  model that's free on OpenCode for a limited time. The team is using this time to
  collect feedback and improve the model.” No calendar end date is given for any
  of them. `https://opencode.ai/inference/v1/models` listed 104 models with the
  `*-free` IDs priced `0` on input and output; a keyless chat request returned
  `429 {"type":"error","error":{"type":"FreeUsageLimitError",…}}`.
- **N4** [NVIDIA gemma-4-31b-it](https://build.nvidia.com/google/gemma-4-31b-it)
  (retrieved 2026-11-23): “Start building with a free API endpoint.” Model
  availability table: “Free Endpoint Available Partner Endpoint Available Download
  Available”. “Context Length 262K”, and the tab is titled “Model by Google |
  NVIDIA NIM”. Endpoint used in the sample code:
  `https://integrate.api.nvidia.com/v1/chat/completions` with
  `Authorization: Bearer $NVIDIA_API_KEY`. The quickstart [N1] still states no
  numerical rate limit, so the ~40 RPM figure remains third-party-reported.
- **F2** [freemodel.in pricing](https://freemodel.in/pricing) and
  [freemodel.in](https://freemodel.in/) (retrieved 2026-11-23): “Free 1M tokens
  every month · no card needed”. Free plan data: “1M tokens / month”, “60 requests
  / min”, “All chat models”. “Tokens cover both your prompt and the model's reply,
  combined. Your plan allowance is spent first and resets monthly; once it is used
  up, requests pause with a clear HTTP 402”. “+ base_url="https://freemodel.in/v1"”
  from the OpenAI-SDK diff. `https://freemodel.in/v1/models` returned 200 with 29
  models including `claude-fable-5`; no generation was attempted (key required).
  The page states no data-retention policy.

## What current llmux2 can configure

- **Explicit keyless deployments:** set `auth = "none"` and
  `provider = "openai_compat"` (or `"openai"`) on a deployment, with no
  `credential`. The executor supplies `AuthMaterial::None`, and the adapter sends
  no Authorization header. A missing credential without opt-in is still a load
  error, as are ambiguous credential/auth/provider combinations. Keyless is
  currently restricted to OpenAI-compatible adapters; keyed config is unchanged.
  Keyless and keyed deployments share model routing and fallback but retain
  separate RPM/concurrency limits, cooldowns and anonymous auth-failure state.
  Their upstream account/IP policies still apply across gateway instances.
- **DeepAI text requests:** Use one user message with text content and set
  `stream` to false or omit it. The official text endpoint accepts a single
  text input and does not expose Chat Completions options. The adapter rejects
  unsupported options instead of ignoring them.
- **Credential headers:** A `deepai` API-key credential sets `Api-Key`. An
  `openai_compat` API-key credential sets Bearer authorization.
- **Per-deployment upstream headers are configurable:** `headers` on
  `[[models.deployments]]` sets extra headers, e.g. OpenRouter's optional
  `HTTP-Referer` and `X-Title` attribution, and accepts `env:`/`file:`
  references for the value. Hop-by-hop names and a keyed deployment's own
  `Authorization`/`x-api-key` are rejected, as are duplicate, invalid, or
  control-character entries, so do not set the credential header again there.
- **Kinds:** the server registry includes `deepai` for the official text API.
  `/v1/responses` is an **inbound** route converted for existing adapters, not a
  native upstream `provider = "responses"` implementation. A config string parsing
  does not mean a provider kind exists in the server registry.
- **Local quotas:** `quota` accepts `rpm`, `rpd`, `tpm`, `tpd` and `concurrent`,
  each a sliding window (requests or estimated tokens per minute or day). Numbers
  must be positive. There is still no **monthly** window, no rolling calendar
  reset, and no money ceiling: a provider that grants a monthly token allowance
  or a free spending cap has to be enforced upstream. Keep paid billing off where
  possible and monitor upstream limits. Do not use gateway fallback to bypass
  account/IP policies.

## Ready-to-paste configuration

Each block is a **separate complete `llmux.toml`**, not a fragment to concatenate.
The four 2026-11-23 offers also exist combined, as
[`examples/llmux.free-providers.example.toml`](../examples/llmux.free-providers.example.toml).
Export `LLMUX_CLIENT_KEY` and the named upstream key first; llmux2 accepts only
`env:`/`file:` secret references, not inline secrets. Free tier eligibility is
your responsibility, not the config’s.

Use only non-sensitive test prompts initially. Syntactic/runtime-config validation
does not verify a live key, current free eligibility, model access, or generation.
For Google, select a model shown as free in your existing project's AI Studio;
the placeholder below is deliberate. For SambaNova, substitute the console's
current base URL. Neither placeholder is environment-variable interpolation:
llmux2 resolves `env:` for secrets only, not URLs/model IDs.

### Cloudflare Workers AI

Replace `ACCOUNT_ID` with the Cloudflare account ID; token must have Workers AI
inference permission for that account. Stay on Workers Free for a hard free cap.

```toml
[server]
client_keys = ["env:LLMUX_CLIENT_KEY"]

[[credentials]]
id = "cloudflare-key"
kind = "api_key"
provider = "openai_compat"
key = "env:CLOUDFLARE_API_TOKEN"

[[models]]
name = "free-cloudflare"
[[models.deployments]]
id = "cloudflare"
credential = "cloudflare-key"
base_url = "https://api.cloudflare.com/client/v4/accounts/ACCOUNT_ID/ai/v1"
upstream_model = "@cf/meta/llama-3.1-8b-instruct"
```

### Kilo Gateway (official API key)

```toml
[server]
client_keys = ["env:LLMUX_CLIENT_KEY"]

[[credentials]]
id = "kilo-key"
kind = "api_key"
provider = "openai_compat"
key = "env:KILO_API_KEY"

[[models]]
name = "free-kilo"
[[models.deployments]]
id = "kilo"
credential = "kilo-key"
base_url = "https://ai-gateway.kilo.ai/api/v1"
upstream_model = "kilo-auto/free"
```

### OpenRouter (free router only)

```toml
[server]
client_keys = ["env:LLMUX_CLIENT_KEY"]

[[credentials]]
id = "openrouter-key"
kind = "api_key"
provider = "openai_compat"
key = "env:OPENROUTER_API_KEY"

[[models]]
name = "free-openrouter"
[[models.deployments]]
id = "openrouter"
credential = "openrouter-key"
base_url = "https://openrouter.ai/api/v1"
upstream_model = "openrouter/free"
quota = { rpm = 20 }
```

### Google AI Studio (coordinate with existing mapping)

Replace `FREE_MODEL_FROM_YOUR_PROJECT` with a currently free-eligible model ID;
do not reuse the consumer Antigravity credentials. Multiple keys in one project
share quotas. Project limits are not encoded here.

```toml
[server]
client_keys = ["env:LLMUX_CLIENT_KEY"]

[[credentials]]
id = "gemini-key"
kind = "api_key"
provider = "openai_compat"
key = "env:GEMINI_API_KEY"

[[models]]
name = "free-gemini"
[[models.deployments]]
id = "gemini"
credential = "gemini-key"
base_url = "https://generativelanguage.googleapis.com/v1beta/openai"
upstream_model = "FREE_MODEL_FROM_YOUR_PROJECT"
```

### Kilo Gateway (mixed anonymous and keyed)

One public model uses anonymous access first, then its official API key after a
429 or another eligible failure. Only the keyed deployment needs an upstream
secret. No fake Bearer token is sent for anonymous requests. Hourly IP limits and
keyed account quotas remain upstream policies; this is not permission to evade
them; llmux2 does not gate paid models, so choose carefully.

```toml
[server]
client_keys = ["env:LLMUX_CLIENT_KEY"]

[[credentials]]
id = "kilo-key"
kind = "api_key"
provider = "openai_compat"
key = "env:KILO_API_KEY"

[[models]]
name = "free-kilo-mixed"
[[models.deployments]]
id = "kilo-anonymous"
auth = "none"
provider = "openai_compat"
base_url = "https://ai-gateway.kilo.ai/api/v1"
upstream_model = "kilo-auto/free"
priority = 0

[[models.deployments]]
id = "kilo-keyed"
credential = "kilo-key"
base_url = "https://ai-gateway.kilo.ai/api/v1"
upstream_model = "kilo-auto/free"
priority = 10
```

### AI Horde (official anonymous access)

No upstream key is needed: the official chat front selects the anonymous key
when Authorization is omitted [H4]. For registered access instead, use an
`api_key` credential through an env/file reference. Refresh the live model
catalog before use; expect queues and worker turnover.

```toml
[server]
client_keys = ["env:LLMUX_CLIENT_KEY"]
request_timeout_ms = 300000

[[models]]
name = "free-horde"
[[models.deployments]]
id = "horde"
auth = "none"
provider = "openai_compat"
base_url = "https://oai.aihorde.net/v1"
upstream_model = "koboldcpp/Artemis-31B-v1.2"
quota = { concurrent = 1 }
```

### SambaNova (evaluation only)

Replace the entire URL `https://YOUR_CONSOLE_HOST/v1` with the console-issued
base URL; the current quickstart does not verify a fixed public URL. This block
is a parseable template, not a verified endpoint. Do not use for production.

```toml
[server]
client_keys = ["env:LLMUX_CLIENT_KEY"]

[[credentials]]
id = "samba-key"
kind = "api_key"
provider = "openai_compat"
key = "env:SAMBANOVA_API_KEY"

[[models]]
name = "free-samba-eval"
[[models.deployments]]
id = "samba"
credential = "samba-key"
base_url = "https://YOUR_CONSOLE_HOST/v1"
upstream_model = "gpt-oss-120b"
quota = { rpm = 20 }
```

### Cohere (evaluation only)

```toml
[server]
client_keys = ["env:LLMUX_CLIENT_KEY"]

[[credentials]]
id = "cohere-key"
kind = "api_key"
provider = "openai_compat"
key = "env:COHERE_API_KEY"

[[models]]
name = "free-cohere-eval"
[[models.deployments]]
id = "cohere"
credential = "cohere-key"
base_url = "https://api.cohere.ai/compatibility/v1"
upstream_model = "command-a-plus-05-2026"
quota = { rpm = 20 }
```

### packet.ai Token Factory (promotion, ends 2026-12-16)

Both models are `$0` until 2026-12-16 and then bill per token, so the expiry is
set to the last second of that day. The page states no rate limit; the `rpm = 20`
here is a guess, not a documented number. A wrong upstream model ID fails as an
upstream error, not as a config error.

```toml
[server]
client_keys = ["env:LLMUX_CLIENT_KEY"]

[[credentials]]
id = "packet-key"
kind = "api_key"
provider = "openai_compat"
key = "env:PACKET_API_KEY"

[[models]]
name = "deepseek-v4.1-flash"
capabilities = { stream = true, max_context = 131072 }
[[models.deployments]]
id = "packet"
credential = "packet-key"
base_url = "https://api.packet.ai/v1"
upstream_model = "nvidia/DeepSeek-V4.1-Flash-NVFP4"
timeout_ms = 60000
quota = { rpm = 20 }
expires_at = 2026-12-16T23:59:59Z
```

### OpenCode Zen free pool (no stated end date)

Free-tier models are documented for the OpenCode client, so an outside caller may
be refused with `429 FreeUsageLimitError`, which `llmux2` treats as rate limiting
and cools down. Only DeepSeek V4.1 Flash Free and Hy4 Flash Free are documented as
zero-retention; Big Pickle's page only promises feedback collection, so assume it
may train on prompts.

```toml
[server]
client_keys = ["env:LLMUX_CLIENT_KEY"]

[[credentials]]
id = "zen-key"
kind = "api_key"
provider = "openai_compat"
key = "env:OPENCODE_ZEN_API_KEY"

[[models]]
name = "big-pickle"
capabilities = { stream = true, max_context = 200000 }
[[models.deployments]]
id = "zen"
credential = "zen-key"
base_url = "https://opencode.ai/inference/openai/v1"
upstream_model = "big-pickle"
timeout_ms = 60000
```

### NVIDIA NIM (free endpoint, per-model)

The model page says “Free Endpoint Available”, but the quickstart publishes no
numerical limit, so `rpm = 20` is a guard, not a verified quota. Per-model
availability varies: the Llama 3.1 8B page still reports its free endpoint as
deprecated.

```toml
[server]
client_keys = ["env:LLMUX_CLIENT_KEY"]

[[credentials]]
id = "nvidia-key"
kind = "api_key"
provider = "openai_compat"
key = "env:NVIDIA_API_KEY"

[[models]]
name = "gemma-4-31b"
capabilities = { stream = true }
[[models.deployments]]
id = "nvidia"
credential = "nvidia-key"
base_url = "https://integrate.api.nvidia.com/v1"
upstream_model = "google/gemma-4-31b-it"
timeout_ms = 60000
quota = { rpm = 20 }
```

### freemodel.in Free plan (monthly token allowance)

The Free plan grants 1M tokens a month with prompt and reply counted together,
plus 60 RPM and 100K TPM. `quota` has no monthly window, so the monthly cap is
left to the upstream `402`, which `llmux2` maps to `quota_exhausted`. The `rpm`
here is set well under the plan's 60 to leave room for the gateway's own retries.

```toml
[server]
client_keys = ["env:LLMUX_CLIENT_KEY"]

[[credentials]]
id = "freemodel-key"
kind = "api_key"
provider = "openai_compat"
key = "env:FREEMODEL_API_KEY"

[[models]]
name = "claude-fable-5"
capabilities = { stream = true, max_context = 1000000 }
[[models.deployments]]
id = "freemodel"
credential = "freemodel-key"
base_url = "https://freemodel.in/v1"
upstream_model = "claude-fable-5"
timeout_ms = 60000
quota = { rpm = 10 }
```

Groq is a conditional candidate, not a verified free recommendation in this
snapshot. Its real config shape is already demonstrated in
[`examples/llmux.example.toml`](../examples/llmux.example.toml); confirm account
free eligibility and current free limits before adopting it. No snippets are
provided for providers whose current offer is paid, retired, provenance-unclear,
or otherwise not established as free.

### Devin SWE-2 (devin_acp bridge)

Not an HTTP provider: the gateway spawns `devin acp --agent-type summarizer`
per request over ACP on stdio. `swe-2-high` is marked Free by `devin models`;
`swe-2-high-lightning` is paid. The CLI must be installed on the gateway host
(LLMUX_DEVIN_BIN) and signed in under a dedicated home (LLMUX_DEVIN_HOME,
default `$HOME/.devin-sandbox`).

```toml
[[models]]
name = "swe-2"
capabilities = { stream = true, max_context = 262144 }

[[models.deployments]]
id = "devin-swe2-high"
auth = "none"
provider = "devin_acp"
upstream_model = "swe-2-high"
priority = 0
timeout_ms = 120000
```

## Validation performed

All eight HTTP-provider TOML blocks were extracted verbatim into separate temporary config
files outside the repository (the Devin `devin_acp` block below was checked with
`llmux-server --check` instead, since it is not an `openai_compat` snippet). A small external Rust harness depending on this
checkout's `llmux-config` called `RuntimeConfig::load` for every file, supplying
non-secret printable placeholder environment values and asserting one model,
the expected deployments, and `openai_compat`. No production credentials or upstream
inference calls were used. Output:

```text
PASS free-cloudflare: 1 model, 1 deployment(s), openai_compat
PASS free-kilo: 1 model, 1 deployment(s), openai_compat
PASS free-openrouter: 1 model, 1 deployment(s), openai_compat
PASS free-gemini: 1 model, 1 deployment(s), openai_compat
PASS free-kilo-mixed: 1 model, 2 deployment(s), openai_compat
PASS free-horde: 1 model, 1 deployment(s), openai_compat
PASS free-samba-eval: 1 model, 1 deployment(s), openai_compat
PASS free-cohere-eval: 1 model, 1 deployment(s), openai_compat
Validated 8 documentation snippets with RuntimeConfig::load (placeholder env secrets; no upstream calls).
```

Parser tests within `cargo test --locked --workspace`:

```text
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

- **DV1** Local `devin models` catalog output on the gateway host:
  `swe-2-high   SWE-2 High   [262K context, Free]` and
  `swe-2-high-lightning   SWE-2 High Lightning   [262K context, $1 / 1M Input, new]`.
  The Devin block above was validated the same way: `llmux-server --check` on a
  minimal config carrying only the `swe-2` model and its `devin_acp` deployment
  passed. The catalog is fetched live by the gateway; a stale upstream id is
  skipped like a cooled deployment (see the routing spec).

This validates config loading only: placeholders, actual account entitlements,
live generations, upstream payload edge cases and UI rendering were not tested.
The keyless implementation also has loopback tests asserting absent upstream
Authorization and mixed-auth fallback with independent cooldown/RPM limits.
