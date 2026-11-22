use crate::{string, ImportOptions};
use llmux_types::provider::DEEPAI_TEXT_MODELS;
use serde_json::Value;
use std::{collections::HashSet, fs, path::Path};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ModelPolicy {
    Any,
    Workers,
    KiloFree,
    DeepAiText,
}

pub(super) struct External {
    pub reference: String,
    pub base_url: String,
}

/// Default requests-per-minute cap for imported Workers AI deployments,
/// overridable with `--workers-ai-rpm`.
pub(super) const WORKERS_DEFAULT_RPM: u32 = 30;

/// Hard daily cap on estimated input tokens for the shared Workers AI
/// credential, bound across every deployment on the account. Sized so a day
/// of worst-case billed usage stays under the documented 10,000-neuron free
/// daily allowance even when output (including hidden reasoning tokens) is
/// 5x the admitted input: the most expensive allowed model
/// (`@cf/qwen/qwen3.8-27b`) bills 290,909 neurons/M output tokens, and
/// 6,500 * (0.040909 + 5 * 0.290909) is about 9,720 neurons.
pub(super) const WORKERS_TPD: u32 = 6_500;

pub(super) struct Policy {
    pub workers: Option<External>,
    pub workers_rpm: u32,
    workers_models: Option<HashSet<String>>,
    pub kilo_key: Option<String>,
    pub kilo_free: Option<HashSet<String>>,
    pub openrouter_replacements: bool,
    pub devin_free: Option<HashSet<String>>,
}

fn reference(path: &Path) -> Result<String, String> {
    let path =
        std::path::absolute(path).map_err(|_| "could not resolve external credential path")?;
    let path = path
        .to_str()
        .ok_or("external credential path must be UTF-8")?;
    if path.contains(['\n', '\r']) {
        return Err("invalid external credential path".into());
    }
    Ok(format!("file:{path}"))
}

pub(super) fn zero(value: &Value) -> bool {
    if let Some(number) = value.as_number() {
        return number.as_u64() == Some(0);
    }
    value.as_str().is_some_and(|s| {
        let mut dots = 0;
        !s.is_empty()
            && s.bytes().any(|b| b == b'0')
            && s.bytes().all(|b| match b {
                b'0' => true,
                b'.' => {
                    dots += 1;
                    dots == 1
                }
                _ => false,
            })
    })
}

impl Policy {
    pub fn load(options: &ImportOptions<'_>) -> Result<Self, String> {
        let workers = match (options.cloudflare_account_id_file, options.cloudflare_token_file) {
            (None, None) => None,
            (Some(account), Some(token)) => {
                let account = fs::read_to_string(account).map_err(|_| "could not read Cloudflare account ID file")?;
                let account = account.trim();
                if account.len() != 32 || !account.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err("Cloudflare account ID must contain 32 hexadecimal characters".into());
                }
                Some(External { reference: reference(token)?, base_url: format!("https://api.cloudflare.com/client/v4/accounts/{account}/ai/v1") })
            }
            _ => return Err("Cloudflare opt-in requires both --cloudflare-account-id-file and --cloudflare-token-file".into()),
        };
        let workers_models = options
            .workers_ai_models
            .map(|path| {
                let content = fs::read_to_string(path)
                    .map_err(|_| "could not read Workers AI model snapshot")?;
                let catalog: Value = serde_json::from_str(&content)
                    .map_err(|_| "invalid Workers AI model snapshot JSON")?;
                if catalog["success"] != true {
                    return Err("Workers AI model snapshot must be a successful API response");
                }
                let rows = catalog["result"]
                    .as_array()
                    .ok_or("Workers AI model snapshot requires a result array")?;
                rows.iter()
                    .map(|row| {
                        row["name"]
                            .as_str()
                            .filter(|name| !name.is_empty())
                            .map(str::to_owned)
                            .ok_or("Workers AI model snapshot requires a name on every model")
                    })
                    .collect::<Result<HashSet<_>, _>>()
            })
            .transpose()?;
        let kilo_free = options
            .kilo_free_models
            .map(|path| {
                let content =
                    fs::read_to_string(path).map_err(|_| "could not read Kilo free-model file")?;
                let catalog: Value =
                    serde_json::from_str(&content).map_err(|_| "invalid Kilo free-model JSON")?;
                let rows = catalog
                    .as_array()
                    .or_else(|| catalog["data"].as_array())
                    .ok_or("Kilo free-model JSON must be an array or a /models object with data")?;
                let mut free = HashSet::new();
                let mut rejected = HashSet::new();
                for row in rows {
                    let id = string(row, "id");
                    if id.is_empty() {
                        continue;
                    }
                    if zero(&row["pricing"]["prompt"]) && zero(&row["pricing"]["completion"]) {
                        free.insert(id.to_owned());
                    } else {
                        rejected.insert(id.to_owned());
                    }
                }
                free.retain(|id| !rejected.contains(id));
                Ok::<_, String>(free)
            })
            .transpose()?;
        if options.workers_ai_rpm.is_some() && workers.is_none() {
            return Err(
                "--workers-ai-rpm requires --cloudflare-account-id-file and --cloudflare-token-file"
                    .into(),
            );
        }
        Ok(Self {
            workers,
            workers_rpm: options.workers_ai_rpm.unwrap_or(WORKERS_DEFAULT_RPM),
            workers_models,
            kilo_free,
            kilo_key: options.kilo_key_file.map(reference).transpose()?,
            openrouter_replacements: options.openrouter_replacements,
            devin_free: options
                .devin_models
                .map(crate::devin::free_models)
                .transpose()?,
        })
    }

    pub fn model(&self, policy: ModelPolicy, model: &str) -> Result<String, &'static str> {
        match policy {
            ModelPolicy::Any => Ok(model.to_owned()),
            ModelPolicy::DeepAiText => DEEPAI_TEXT_MODELS
                .contains(&model)
                .then(|| model.to_owned())
                .ok_or("unsupported DeepAI text model"),
            ModelPolicy::Workers => {
                let mapped = workers_model(model)
                    .ok_or("unknown or deprecated Workers AI model (or requires paid access)")?;
                if self
                    .workers_models
                    .as_ref()
                    .is_some_and(|models| !models.contains(mapped))
                {
                    return Err("mapped model not present in Workers AI model snapshot");
                }
                Ok(mapped.to_owned())
            }
            ModelPolicy::KiloFree => match &self.kilo_free {
                None => Err("keyed Kilo requires --kilo-free-models; skipped to avoid cost"),
                Some(free) if free.contains(model) => Ok(model.to_owned()),
                Some(_) => Err("Kilo model not verified free for both prompt and completion"),
            },
        }
    }
}

pub(super) fn cloudflare(provider: &str) -> bool {
    matches!(
        provider,
        "cloudflare-direct" | "cfpd" | "cloudflare-playground" | "cfp"
    )
}

pub(super) fn kilo(provider: &str) -> bool {
    matches!(provider, "kilo" | "kilocode")
}

pub(super) fn workers_model(raw: &str) -> Option<&'static str> {
    let mut model = raw;
    while let Some((prefix, rest)) = model.split_once('/') {
        if cloudflare(prefix) || prefix == "@cf" {
            model = rest;
        } else {
            break;
        }
    }
    if model == "meta-llama/llama-3.3-70b-instruct-fp8-fast" {
        model = "meta/llama-3.3-70b-instruct-fp8-fast";
    }
    // https://developers.cloudflare.com/workers-ai/changelog/#2026-05-08
    // https://developers.cloudflare.com/workers-ai/models/granite-4.0-h-micro/
    // https://developers.cloudflare.com/workers-ai/platform/pricing/
    if model == "meta/llama-3.1-8b-instruct" {
        model = "ibm-granite/granite-4.0-h-micro";
    }
    WORKERS
        .iter()
        .find(|id| id.strip_prefix("@cf/") == Some(model))
        .copied()
}

const WORKERS: &[&str] = &[
    // https://developers.cloudflare.com/workers-ai/models/gemma-4-26b-a4b-it/
    "@cf/google/gemma-4-26b-a4b-it",
    // https://developers.cloudflare.com/workers-ai/models/gpt-oss-20b/
    "@cf/openai/gpt-oss-20b",
    // https://developers.cloudflare.com/workers-ai/models/gpt-oss-120b/
    "@cf/openai/gpt-oss-120b",
    // https://developers.cloudflare.com/workers-ai/models/mistral-small-3.1-24b-instruct/
    "@cf/mistralai/mistral-small-3.1-24b-instruct",
    // https://developers.cloudflare.com/workers-ai/models/qwen3.8-27b/
    "@cf/qwen/qwen3.8-27b",
    // https://developers.cloudflare.com/workers-ai/models/llama-3.3-70b-instruct-fp8-fast/
    "@cf/meta/llama-3.3-70b-instruct-fp8-fast",
    // https://developers.cloudflare.com/workers-ai/models/llama-4-scout-17b-16e-instruct/
    "@cf/meta/llama-4-scout-17b-16e-instruct",
    // https://developers.cloudflare.com/workers-ai/models/qwen2.5-coder-32b-instruct/
    "@cf/qwen/qwen2.5-coder-32b-instruct",
    // https://developers.cloudflare.com/workers-ai/models/granite-4.0-h-micro/
    "@cf/ibm-granite/granite-4.0-h-micro",
];

/// Upstream model IDs that cannot serve `/v1/chat/completions`: embedding or
/// typed-endpoint models the export lists as combo legs. llmux2 serves only
/// chat-class endpoints, so these are classified out at emit rather than
/// advertised as dead routes.
// https://docs.mistral.ai/capabilities/embeddings/overview/ (mistral-embed, codestral-embed)
// respan endpoint-type rejection observed on OCI probe sweep 2026-11-22 (span-01-lite)
pub(super) const NON_CHAT_MODELS: &[&str] = &["codestral-embed", "mistral-embed", "span-01-lite"];

/// Whether an emitted upstream model id resolves to a reviewed non-chat
/// model. Gateway-routed ids keep their routing path — e.g. a leg through
/// the respan node emits `openrouter/respan/span-01-lite` — so the match is
/// on the leaf segment, with any `:variant` suffix (`:free`, `:batch`)
/// stripped. The non-chat property belongs to the model itself, so a routed
/// `…/mistral-embed` drops too, while a different leaf (e.g.
/// `openrouter/respan/span-01-lite-v2` or `openrouter/openai/gpt-4o`) is
/// unaffected.
pub(super) fn non_chat(model: &str) -> bool {
    let leaf = model.rsplit('/').next().unwrap_or(model);
    let leaf = leaf.split_once(':').map_or(leaf, |(base, _)| base);
    NON_CHAT_MODELS.contains(&leaf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn non_chat_classification_matches_routed_leaf_and_variant() {
        for id in [
            "span-01-lite",
            "openrouter/respan/span-01-lite",
            "openrouter/respan/span-01-lite:free",
            "mistral-embed",
            "openrouter/mistralai/codestral-embed",
        ] {
            assert!(non_chat(id), "{id}");
        }
        for id in [
            "",
            "openrouter/openai/gpt-4o",
            "openrouter/respan/span-01-lite-v2",
            "span-01-lite-plus",
            "openrouter/",
            ":free",
        ] {
            assert!(!non_chat(id), "{id}");
        }
    }

    #[test]
    fn cost_metadata_is_exact_and_does_not_accept_float_underflow() {
        for price in [json!(0), json!("0"), json!("0.00000")] {
            assert!(zero(&price));
        }
        for price in [
            json!(null),
            json!(false),
            json!("free"),
            json!("1e-999"),
            json!("0.000000000000001"),
            json!(0.0),
            json!(-1),
            json!("0..0"),
        ] {
            assert!(!zero(&price));
        }
    }

    #[test]
    fn workers_allowlist_uses_exact_official_ids_and_never_revives_deprecated_models() {
        for id in WORKERS {
            assert_eq!(workers_model(id), Some(*id));
        }
        assert_eq!(
            workers_model("meta-llama/llama-3.3-70b-instruct-fp8-fast"),
            Some("@cf/meta/llama-3.3-70b-instruct-fp8-fast")
        );
        for model in [
            "",
            "meta/llama-3.2-1b-instruct",
            "meta/llama-3.2-3b-instruct",
            "meta/llama-3.1-8b-instruct-fp8",
            "qwen/qwen3-30b-a3b-fp8",
            "moonshotai/kimi-k2.6",
            "deepseek-ai/deepseek-v4-flash-0731",
            "zai-org/glm-5.3",
        ] {
            assert!(workers_model(model).is_none(), "{model}");
        }
    }

    /// Neurons billed per million input/output tokens for each allowlisted
    /// model, from https://developers.cloudflare.com/workers-ai/platform/pricing/.
    /// The shared credential's `WORKERS_TPD` must keep a day of worst-case
    /// billed usage — admitted input plus output (including hidden reasoning
    /// tokens) up to 5x larger — under the 10,000-neuron free daily allowance.
    /// Adding a pricier model without lowering the cap fails this test.
    const WORKERS_NEURON_RATES: &[(&str, u64, u64)] = &[
        ("@cf/google/gemma-4-26b-a4b-it", 9_091, 27_273),
        ("@cf/openai/gpt-oss-20b", 18_182, 27_273),
        ("@cf/openai/gpt-oss-120b", 31_818, 68_182),
        (
            "@cf/mistralai/mistral-small-3.1-24b-instruct",
            31_876,
            50_488,
        ),
        ("@cf/qwen/qwen3.8-27b", 40_909, 290_909),
        ("@cf/meta/llama-3.3-70b-instruct-fp8-fast", 26_668, 204_805),
        ("@cf/meta/llama-4-scout-17b-16e-instruct", 24_545, 77_273),
        ("@cf/qwen/qwen2.5-coder-32b-instruct", 60_000, 90_909),
        ("@cf/ibm-granite/granite-4.0-h-micro", 1_542, 10_158),
    ];

    #[test]
    fn workers_daily_cap_stays_under_the_free_neuron_allowance() {
        let mut rated: Vec<_> = WORKERS_NEURON_RATES.iter().map(|(id, ..)| *id).collect();
        let mut allowed: Vec<_> = WORKERS.to_vec();
        rated.sort();
        allowed.sort();
        assert_eq!(allowed, rated, "every allowed model needs a neuron rate");
        let worst_per_million = WORKERS_NEURON_RATES
            .iter()
            .map(|(_, input, output)| input + 5 * output)
            .max()
            .unwrap();
        let neurons_per_day = u64::from(WORKERS_TPD) * worst_per_million / 1_000_000;
        assert!(
            neurons_per_day < 10_000,
            "WORKERS_TPD bills {neurons_per_day} neurons/day at the worst allowed \
             model (1,495,454 neurons/M for qwen3.8-27b); the free allowance is 10,000"
        );
    }
}
