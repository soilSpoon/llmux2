use crate::{policy::ModelPolicy, Connection};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(super) type Route = (&'static str, &'static str, &'static str);

pub(super) fn find(model: &str) -> Option<&'static Route> {
    ROUTES.iter().find(|row| row.0 == model)
}

pub(super) fn antigravity(model: &str) -> Option<&'static Route> {
    ANTIGRAVITY_ROUTES.iter().find(|row| row.0 == model)
}

pub(super) fn antigravity_connection(route: &Route, connection: &Connection) -> bool {
    connection.provider.is_builtin("gemini")
        && connection.credential.is_some()
        && matches(route, connection)
}

pub(super) fn seed_antigravity(direct: &mut BTreeMap<String, Value>, connections: &[Connection]) {
    for route in ANTIGRAVITY_ROUTES {
        if connections
            .iter()
            .any(|connection| antigravity_connection(route, connection))
        {
            let name = format!("antigravity/{}", route.0);
            direct.entry(name.clone()).or_insert_with(|| json!({"name":name,"models":[crate::upstream::ProviderIdentity::Builtin("antigravity".into()).step(route.0)]}));
        }
    }
}

const ANTIGRAVITY_ROUTES: &[Route] = &[
    // Operator verified gemini-3-flash-preview via OCI Gemini models.list.
    // https://ai.google.dev/api/models#method:-models.list
    ("gemini-3-flash", "gemini", "gemini-3-flash-preview"),
    // OCI verified all five Gemini keys list this GA model; generateContent returned
    // 200 with modelVersion gemini-3.1-flash-lite (operator-provided verification).
    // https://ai.google.dev/api/models#method:-models.list
    ("gemini-3.1-flash-lite", "gemini", "gemini-3.1-flash-lite"),
];

pub(super) fn matches(route: &Route, connection: &Connection) -> bool {
    if route.1 != "anthropic" && !matches!(connection.kind.as_str(), "openai" | "openai_compat") {
        return false;
    }
    match route.1 {
        "anthropic" => {
            connection.kind == "anthropic" && connection.base_url == "https://api.anthropic.com/v1"
        }
        "gemini" => {
            connection.base_url == "https://generativelanguage.googleapis.com/v1beta/openai"
        }
        "mistral" => connection.base_url == "https://api.mistral.ai/v1",
        "workers" => {
            connection.provider.is_builtin("workers-ai")
                && connection.policy == ModelPolicy::Workers
        }
        "kilo" => {
            connection.base_url == "https://ai-gateway.kilo.ai/api/v1"
                && (connection.credential.is_none() || connection.policy == ModelPolicy::KiloFree)
        }
        _ => false,
    }
}

pub(super) fn seed(direct: &mut BTreeMap<String, Value>, connections: &[Connection]) {
    for route in ROUTES {
        if connections
            .iter()
            .any(|connection| matches(route, connection))
        {
            let name = format!("openrouter/{}", route.0);
            direct.entry(name.clone()).or_insert_with(|| json!({"name":name,"models":[crate::upstream::ProviderIdentity::Builtin("openrouter".into()).step(route.0)]}));
        }
    }
}

const ROUTES: &[Route] = &[
    // https://platform.claude.com/docs/en/models/sonnet-5-5
    (
        "anthropic/claude-sonnet-5-5",
        "anthropic",
        "claude-sonnet-5-5",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-fable-5.1:batch",
        "anthropic",
        "claude-fable-5-1",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-fable-5.2:batch",
        "anthropic",
        "claude-fable-5-2",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-fable-5:batch",
        "anthropic",
        "claude-fable-5",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-haiku-4.5:batch",
        "anthropic",
        "claude-haiku-4-5-20251001",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-haiku-5.5:batch",
        "anthropic",
        "claude-haiku-5-5",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-opus-4.5:batch",
        "anthropic",
        "claude-opus-4-5-20251101",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-opus-4.6:batch",
        "anthropic",
        "claude-opus-4-6",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-opus-4.7:batch",
        "anthropic",
        "claude-opus-4-7",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-opus-4.8:batch",
        "anthropic",
        "claude-opus-4-8",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-opus-5.5:batch",
        "anthropic",
        "claude-opus-5-5",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-opus-5:batch",
        "anthropic",
        "claude-opus-5",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-sonnet-4.5:batch",
        "anthropic",
        "claude-sonnet-4-5-20250929",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-sonnet-4.6:batch",
        "anthropic",
        "claude-sonnet-4-6",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-sonnet-5.5:batch",
        "anthropic",
        "claude-sonnet-5-5",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-sonnet-5:batch",
        "anthropic",
        "claude-sonnet-5",
    ),
    // https://ai.google.dev/gemini-api/docs/models
    (
        "google/gemini-3-flash-preview:batch",
        "gemini",
        "gemini-3-flash-preview",
    ),
    // https://ai.google.dev/gemini-api/docs/models
    (
        "google/gemini-3.1-flash-lite:batch",
        "gemini",
        "gemini-3.1-flash-lite",
    ),
    // https://ai.google.dev/gemini-api/docs/models
    (
        "google/gemini-3.1-pro-preview:batch",
        "gemini",
        "gemini-3.1-pro-preview",
    ),
    // https://ai.google.dev/gemini-api/docs/models
    (
        "google/gemini-3.5-flash-lite:batch",
        "gemini",
        "gemini-3.5-flash-lite",
    ),
    // https://ai.google.dev/gemini-api/docs/models
    (
        "google/gemini-3.6-flash:batch",
        "gemini",
        "gemini-3.6-flash",
    ),
    // https://ai.google.dev/gemini-api/docs/models
    (
        "google/gemini-3.8-flash:batch",
        "gemini",
        "gemini-3.8-flash",
    ),
    // https://ai.google.dev/gemini-api/docs/models
    ("google/gemini-4-argon:batch", "gemini", "gemini-4-argon"),
    // https://ai.google.dev/gemini-api/docs/models
    ("google/gemini-4-carbon:batch", "gemini", "gemini-4-carbon"),
    // https://docs.mistral.ai/inference/model-selection-guide
    (
        "deepseek/deepseek-v4.1-flash-1017:batch",
        "mistral",
        "deepseek-v4-1-flash-1017",
    ),
    // https://developers.cloudflare.com/workers-ai/models/gpt-oss-120b/
    (
        "openai/gpt-oss-120b:batch",
        "workers",
        "@cf/openai/gpt-oss-120b",
    ),
    // https://developers.cloudflare.com/workers-ai/models/qwen3.8-27b/
    ("qwen/qwen3.8-27b:batch", "workers", "@cf/qwen/qwen3.8-27b"),
    // https://platform.claude.com/docs/en/models/fable-5-2
    (
        "anthropic/claude-fable-5.2",
        "anthropic",
        "claude-fable-5-2",
    ),
    // https://ai.google.dev/gemini-api/docs/models
    ("google/gemini-4-hydrogen", "gemini", "gemini-4-hydrogen"),
    // https://ai-gateway.kilo.ai/api/v1/models
    (
        "baidu/ernie-5.5-flash-preview:free",
        "kilo",
        "baidu/ernie-5.5-flash-preview:free",
    ),
    // https://ai-gateway.kilo.ai/api/v1/models
    (
        "inclusionai/ling-3.2-flash:free",
        "kilo",
        "inclusionai/ling-3.2-flash:free",
    ),
    // https://developers.cloudflare.com/workers-ai/models/gemma-4-26b-a4b-it/
    (
        "google/gemma-4-26b-a4b-it:free",
        "workers",
        "@cf/google/gemma-4-26b-a4b-it",
    ),
    // https://ai.google.dev/gemini-api/docs/models
    ("google/gemini-4-argon", "gemini", "gemini-4-argon"),
    // https://ai.google.dev/gemini-api/docs/models
    ("google/gemini-4-carbon", "gemini", "gemini-4-carbon"),
    // https://ai-gateway.kilo.ai/api/v1/models
    (
        "recursive/flint-v1.0:free",
        "kilo",
        "recursive/flint-v1.0:free",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    ("anthropic/claude-fable-5", "anthropic", "claude-fable-5"),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-fable-5.1",
        "anthropic",
        "claude-fable-5-1",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-haiku-4.5",
        "anthropic",
        "claude-haiku-4-5-20251001",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-haiku-5.5",
        "anthropic",
        "claude-haiku-5-5",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-opus-4.5",
        "anthropic",
        "claude-opus-4-5-20251101",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    ("anthropic/claude-opus-4.6", "anthropic", "claude-opus-4-6"),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    ("anthropic/claude-opus-4.7", "anthropic", "claude-opus-4-7"),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    ("anthropic/claude-opus-4.8", "anthropic", "claude-opus-4-8"),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    ("anthropic/claude-opus-5", "anthropic", "claude-opus-5"),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-sonnet-4.5",
        "anthropic",
        "claude-sonnet-4-5-20250929",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-sonnet-4.6",
        "anthropic",
        "claude-sonnet-4-6",
    ),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    ("anthropic/claude-sonnet-5", "anthropic", "claude-sonnet-5"),
    // https://platform.claude.com/docs/en/about-claude/models/overview
    (
        "anthropic/claude-sonnet-5.5",
        "anthropic",
        "claude-sonnet-5-5",
    ),
    // https://ai.google.dev/gemini-api/docs/models
    ("google/gemini-2.5-flash", "gemini", "gemini-2.5-flash"),
    // https://ai.google.dev/gemini-api/docs/models
    (
        "google/gemini-2.5-flash-lite",
        "gemini",
        "gemini-2.5-flash-lite",
    ),
    // https://ai.google.dev/gemini-api/docs/models
    ("google/gemini-2.5-pro", "gemini", "gemini-2.5-pro"),
    // https://ai.google.dev/gemini-api/docs/models
    (
        "google/gemini-3-flash-preview",
        "gemini",
        "gemini-3-flash-preview",
    ),
    // https://ai.google.dev/gemini-api/docs/models
    (
        "google/gemini-3.1-pro-preview",
        "gemini",
        "gemini-3.1-pro-preview",
    ),
    // https://ai.google.dev/gemini-api/docs/models
    ("google/gemini-3.6-flash", "gemini", "gemini-3.6-flash"),
    // https://ai.google.dev/gemini-api/docs/models
    ("google/gemini-4-neon", "gemini", "gemini-4-neon"),
    // https://ai-gateway.kilo.ai/api/v1/models
    ("liquid/lfm-3-1.7b:free", "kilo", "liquid/lfm-3-1.7b:free"),
    // https://ai-gateway.kilo.ai/api/v1/models
    (
        "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free",
        "kilo",
        "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free",
    ),
    // https://ai-gateway.kilo.ai/api/v1/models
    (
        "nvidia/nemotron-3-ultra-550b-a55b:free",
        "kilo",
        "nvidia/nemotron-3-ultra-550b-a55b:free",
    ),
    // https://ai-gateway.kilo.ai/api/v1/models
    (
        "nvidia/nemotron-3.5-lightning:free",
        "kilo",
        "nvidia/nemotron-3.5-lightning:free",
    ),
    // https://ai-gateway.kilo.ai/api/v1/models
    (
        "nvidia/nemotron-3.5-super-vl:free",
        "kilo",
        "nvidia/nemotron-3.5-super-vl:free",
    ),
    // https://ai-gateway.kilo.ai/api/v1/models
    (
        "particle-ai/particle-1:free",
        "kilo",
        "particle-ai/particle-1:free",
    ),
    // https://docs.mistral.ai/inference/model-selection-guide
    ("mistralai/codestral-2508", "mistral", "codestral-2508"),
    // https://docs.mistral.ai/inference/model-selection-guide
    (
        "mistralai/ministral-14b-2512",
        "mistral",
        "ministral-14b-2512",
    ),
    // https://docs.mistral.ai/inference/model-selection-guide
    (
        "mistralai/ministral-3b-2512",
        "mistral",
        "ministral-3b-2512",
    ),
    // https://docs.mistral.ai/inference/model-selection-guide
    (
        "mistralai/ministral-8b-2512",
        "mistral",
        "ministral-8b-2512",
    ),
    // https://docs.mistral.ai/inference/model-selection-guide
    (
        "mistralai/mistral-small-2603",
        "mistral",
        "mistral-small-2603",
    ),
    // https://docs.mistral.ai/inference/model-selection-guide
    ("mistralai/mistral-large-4-0", "mistral", "mistral-large-4"),
    // https://developers.cloudflare.com/workers-ai/models/mistral-small-3.1-24b-instruct/
    (
        "mistralai/mistral-small-3.1-24b-instruct",
        "workers",
        "@cf/mistralai/mistral-small-3.1-24b-instruct",
    ),
    // https://developers.cloudflare.com/workers-ai/models/qwen3.8-27b/
    ("qwen/qwen3.8-27b", "workers", "@cf/qwen/qwen3.8-27b"),
];
