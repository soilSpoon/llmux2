# Official free API providers

Research snapshot: **2026-11-20**, against llmux2 `eb3b617`. Official pages were
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

## Import gaps → replacements

The dry-run counts are **group totals**, not per-provider counts: 43 bridge legs,
34 OAuth legs (including separately handled Codex), 7 no-auth legs, 1 custom-header
leg. These are not counts of legs that can be restored unchanged.

| Skipped connection | Official replacement | Free status and llmux2 decision |
| --- | --- | --- |
| `cloudflare-direct`, `cloudflare-playground` (43-leg bridge group) | Workers AI account API token, `/client/v4/accounts/ACCOUNT_ID/ai/v1` [CF1–2] | 10,000 neurons/day; `openai_compat`. Use a free-eligible model, not the playground catalog. In particular GLM-5.2 is explicitly paid-method-gated. |
| `deepai` / `deepai-code` bridge (same group) | DeepAI official `https://api.deepai.org/api/<model>` with `api-key` [D1] | No free text/code API allowance verified. Official form/job API is not Chat Completions; not an as-is adapter replacement. Use Kilo, OpenRouter or AI Studio for coding instead. |
| `duckduckgo-web` (same group) | No official third-party Duck.ai inference API verified [D2] | Drop bridge. Substitute Kilo/OpenRouter/Workers AI/AI Studio; these are model-access substitutes, not DuckDuckGo search or privacy-equivalent services. |
| `antigravity` (34-leg OAuth group) | Google AI Studio/Gemini API key [G1–3] | Official free eligible models, project quotas, data-use caveats. `openai_compat`; do not migrate consumer OAuth. Five existing keys are not five independent project quotas. |
| `xai-oauth` / grok-cli (same group) | xAI API key at `https://api.x.ai/v1` [X1–2] | Official keyed access, but no current free credit grant verified. Do not count as a free replacement. Current quickstart demonstrates Responses; llmux2 has no native upstream `responses` kind. Chat compatibility/model eligibility must be checked separately. |
| `kilo`, `kilo-free` (7-leg no-auth group) | Same official gateway with Kilo API key; anonymous free access is also explicitly documented [K1–3] | Both keyed and explicit `auth = "none"` deployments work. Anonymous is 200 requests/hour/IP. |
| `aihorde` (same group) | Official OpenAI-compatible `https://oai.aihorde.net/v1`, Horde key or anonymous access [H1–4] | Omit Authorization: the service selects anonymous key `0000000000`. Works with `openai_compat`; native `https://aihorde.net/api/v2` is asynchronous and is not compatible. Queue/worker capacity, no fixed free token budget verified. |
| `hms` (1 custom-header leg) | Unknown operator/protocol; require the original operator's API/auth documentation | No connection named `hms` was present in the supplied connection list. Custom headers are not configurable. Do not infer legitimacy from a working bridge or import arbitrary headers. |

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
| Conditional: Groq | `https://api.groq.com/openai/v1`; API key [R2] | Current free-plan entitlement and numerical free limits **unverified** in retrieved rate-limit page (account Limits page required) [R1]. `openai/gpt-oss-20b`, `openai/gpt-oss-120b` [R2–3] | Do not copy historical 30 RPM/14,400 RPD into config. Organization-scoped limits [R1]; default no retention with exceptions [R4]. Resale/multi-account terms **unverified**. Chat compatible [R2]. |
| Trial only: Cerebras | `https://api.cerebras.ai/v1`; API key [B2] | $5 credits, verified payment method, expires 30 days; explicitly no permanently free tier [B1]. `gpt-oss-120b`: 5 RPM, 30K uncached TPM, 90K total TPM, 1M TPH/TPD | Not recurring free. `qwen-3.8-27b` also listed; do not infer its separate limits from GPT-OSS. Training/resale/multi-account terms **unverified**. Chat yes. |
| Trial only: Scaleway | Console endpoint; current base URL/auth details **unverified** in fetched quickstart [SC2] | New customers get 1,000,000 tokens, then paid [SC1]; `glm-5.3` listed | Not verified as recurring monthly. OpenAI-compatible per official product page; billing/retention/resale/multi-account details **unverified**. No ready-to-enable config until endpoint/key confirmed. |
| Hold: NVIDIA NIM / build.nvidia.com | `https://integrate.api.nvidia.com/v1`; Developer API key [N1, N3] | No numerical free allowance verified. Selected Llama 3.1 8B page says **Free Endpoint Deprecated** [N2] | Do not assume old 1,000 credits/40 RPM. Official key creation exists, but current free model entitlement and service terms **unverified**. NIM API compatibility alone does not prove hosted free availability. |
| Hold: Mistral La Plateforme / AI Studio | `https://api.mistral.ai/v1`; API key [M1] | Current Experiment/free allowance **unverified**. `mistral-small-latest` official example | Old tier URL returned 404. Current quickstart requires a plan with API access and points to adding a payment method; limits in Admin [M1–2]. Training, resale and multi-account rules **unverified**. Chat yes, but not established free. |
| Exclude: Hugging Face Inference Providers | `https://router.huggingface.co/v1`; HF token | Current free-user credits **None**; paid PRO $2/month [F1]. Example `deepseek-ai/DeepSeek-V3-0324` | Purchasing credits/PRO is not free. Underlying provider/model terms apply; training/resale/multi-account details **unverified**. Chat yes technically. |
| Exclude: Chutes | Current API base/auth/model catalog **unverified** | “We do not offer a free tier at this time.” [CH1] | Pay-as-you-go, not a free replacement. Compatibility, data and resale terms **unverified** in fetched pricing page. |
| Exclude: GitHub Models | No active endpoint/auth | Official docs say fully retired July 30, 2026 [GH1]; no current free models/limits | Do not configure legacy `models.github.ai/inference` or PAT examples. No. |
| Hold: LLM7 | `https://api.llm7.io/v1`; dashboard API token; `default`, `fast`, `pro` selectors [L1] | No current free allowance or limits verified. Homepage advertises lower prices, not free access | First-party docs establish keyed OpenAI client support, not underlying provider authorization. Training/resale/multi-account terms and provenance **unverified**. Technically Chat yes; do not recommend until those are established. |

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
  Website/app free chat is not evidence of a supported free text API. No official
  code/chat bridge protocol or free API limit verified.
- **D2** [DuckDuckGo help URL attempted](https://duckduckgo.com/duckduckgo-help-pages/duckai/ai-chat):
  returned the generic DuckDuckGo shell, not third-party inference documentation.
  No official API was verified; this is not a claim to have proven none exists.
- **X1** [xAI overview](https://docs.x.ai/docs/overview):
  `base_url="https://api.x.ai/v1"`, `model="grok-4.7"` in Responses example;
  navigation says “Purchase credits”.
- **X2** [xAI rate limits](https://docs.x.ai/docs/key-information/consumption-and-rate-limits):
  tiers are “based on total revenue received through prepaid credit purchases or successfully fulfilled invoices.”
  No current free-credit entitlement verified. Do not assume old promotions still apply.

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
- **No arbitrary upstream header configuration:** `RawDeployment` has no header
  field and uses `deny_unknown_fields`; so does the credential schema. The
  adapter's internal `AuthMaterial::Header` and `StaticKey::header` do not expose
  a TOML map. HMS is blocked on both official provenance and configurable headers.
  OpenRouter optional `HTTP-Referer`/title attribution headers cannot be set here;
  they are not required for the normal Bearer-auth request. No implementation changes made.
- **Kinds:** server registry installs `openai`, `openai_compat`, and `anthropic`.
  `/v1/responses` is an **inbound** route converted for existing adapters, not a
  native upstream `provider = "responses"` implementation. A config string parsing
  does not mean a provider kind exists in the server registry.
- **Local quotas:** only `quota.rpm` and `quota.concurrent` are supported; RPD,
  TPM and TPD config values are rejected. None of these snippets can enforce daily
  neurons/tokens or an account-wide free spending ceiling. Keep paid billing off
  where possible and monitor upstream limits. Do not use gateway fallback to
  bypass account/IP policies.

## Ready-to-paste configuration

Each block is a **separate complete `llmux.toml`**, not a fragment to concatenate.
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

Groq is a conditional candidate, not a verified free recommendation in this
snapshot. Its real config shape is already demonstrated in
[`examples/llmux.example.toml`](../examples/llmux.example.toml); confirm account
free eligibility and current free limits before adopting it. No snippets are
provided for providers whose current offer is paid, retired, provenance-unclear,
or otherwise not established as free.

## Validation performed

All eight TOML blocks were extracted verbatim into separate temporary config
files outside the repository. A small external Rust harness depending on this
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

This validates config loading only: placeholders, actual account entitlements,
live generations, upstream payload edge cases and UI rendering were not tested.
The keyless implementation also has loopback tests asserting absent upstream
Authorization and mixed-auth fallback with independent cooldown/RPM limits.
