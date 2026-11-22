use crate::{string, PROVIDERS};
use http::Uri;
use llmux_types::provider::{
    ANTHROPIC as KIND_ANTHROPIC, DEEPAI as KIND_DEEPAI, DEEPAI_TEXT_MODELS,
    DEVIN_ACP as DEVIN_ACP_KIND, DEVIN_ACP_BASE_URL, OPENAI_COMPAT as KIND_OPENAI_COMPAT,
};
use serde_json::{json, Value};
use std::net::IpAddr;

pub(super) const DEEPAI_BASE: &str = "https://api.deepai.org/api";
const DEEPAI_CODE_SIDECAR: &str = "http://127.0.0.1:8790/deepai/v1";

pub(super) const PROVIDER_ALIASES: &[(&str, &str)] = &[
    ("ds", "deepseek"),
    ("kc", "kilocode"),
    ("cfpd", "cloudflare-direct"),
    ("cfp", "cloudflare-playground"),
    ("dva", "devin-cli-agentic"),
    ("xao", "xai-oauth"),
    ("cx", "codex"),
];

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) enum ProviderIdentity {
    Builtin(String),
    Custom(String),
}

impl ProviderIdentity {
    pub(super) fn as_str(&self) -> &str {
        match self {
            Self::Builtin(id) | Self::Custom(id) => id,
        }
    }

    pub(super) fn is_builtin(&self, id: &str) -> bool {
        matches!(self, Self::Builtin(value) if value == id)
    }

    pub(super) fn connection(provider: &str, nodes: &[Value]) -> Self {
        if nodes.iter().any(|node| string(node, "id") == provider) {
            Self::Custom(provider.into())
        } else {
            Self::builtin(provider)
        }
    }

    fn builtin(provider: &str) -> Self {
        Self::Builtin(
            PROVIDER_ALIASES
                .iter()
                .find(|(alias, _)| *alias == provider)
                .map_or(provider, |(_, id)| *id)
                .into(),
        )
    }

    pub(super) fn step(&self, model: &str) -> Value {
        json!({"kind":"model", "_resolvedProviderId":self.as_str(),
            "_resolvedProviderKind":if matches!(self, Self::Custom(_)) {"custom"} else {"builtin"}, "model":model})
    }
}

impl std::fmt::Display for ProviderIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub(super) fn canonical_provider(provider: &str, nodes: &[Value]) -> ProviderIdentity {
    nodes
        .iter()
        .find(|node| string(node, "id") == provider || string(node, "prefix") == provider)
        .map(|node| ProviderIdentity::Custom(string(node, "id").into()))
        .unwrap_or_else(|| ProviderIdentity::builtin(provider))
}

pub(super) fn model_id<'a>(raw: &'a str, provider: &ProviderIdentity, nodes: &[Value]) -> &'a str {
    let node = nodes
        .iter()
        .find(|node| matches!(provider, ProviderIdentity::Custom(id) if id == string(node, "id")));
    let mut model = raw;
    loop {
        let Some((segment, rest)) = model.split_once('/') else {
            return model;
        };
        if segment == provider.as_str()
            || PROVIDER_ALIASES
                .iter()
                .any(|(alias, id)| provider.is_builtin(id) && *alias == segment)
            || node.is_some_and(|node| segment == string(node, "prefix"))
        {
            model = rest;
        } else {
            return model;
        }
        if node.is_none() {
            return model;
        }
    }
}

pub(super) fn model_target<'a>(
    step: &'a Value,
    nodes: &[Value],
) -> Option<(ProviderIdentity, &'a str)> {
    let raw = step
        .as_str()
        .unwrap_or_else(|| string(step, "model"))
        .trim();
    let resolved = string(step, "_resolvedProviderId");
    let provider_id = match resolved {
        "" => match string(step, "providerId") {
            "" => string(step, "provider"),
            id => id,
        },
        id => id,
    };
    let (provider, model) = if provider_id.is_empty() {
        raw.split_once('/')?
    } else {
        (provider_id, raw)
    };
    let canonical = if resolved.is_empty() {
        canonical_provider(provider, nodes)
    } else {
        match string(step, "_resolvedProviderKind") {
            "custom" => ProviderIdentity::Custom(resolved.into()),
            _ => ProviderIdentity::Builtin(resolved.into()),
        }
    };
    let model = if resolved.is_empty()
        && (!provider_id.is_empty() || matches!(canonical, ProviderIdentity::Custom(_)))
    {
        model_id(model, &canonical, nodes)
    } else {
        model
    };
    let canonical = if resolved.is_empty()
        && provider == KIND_DEEPAI
        && DEEPAI_TEXT_MODELS.contains(&model)
        && matches!(&canonical, ProviderIdentity::Custom(id) if id != provider)
    {
        ProviderIdentity::builtin(provider)
    } else {
        canonical
    };
    if provider.is_empty()
        || model.is_empty()
        || model.contains('*')
        || string(step, "kind") == "provider-wildcard"
    {
        return None;
    }
    Some((canonical, model))
}

fn local_host(url: &str, include_unspecified: bool) -> bool {
    let Ok(uri) = url.parse::<Uri>() else {
        return false;
    };
    let host = uri
        .host()
        .unwrap_or("")
        .trim_matches(['[', ']'])
        .trim_end_matches('.');
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback() || include_unspecified && ip.is_unspecified(),
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback()
                || include_unspecified && ip.is_unspecified()
                || ip.to_ipv4_mapped().is_some_and(|ip| {
                    ip.is_loopback() || include_unspecified && ip.is_unspecified()
                })
        }
        Err(_) => false,
    }
}

pub(super) fn metadata(connection: &Value) -> Result<Value, &'static str> {
    match connection.get("providerSpecificData") {
        None | Some(Value::Null) => Ok(json!({})),
        Some(Value::Object(_)) => Ok(connection["providerSpecificData"].clone()),
        Some(Value::String(text)) => {
            let parsed: Value =
                serde_json::from_str(text).map_err(|_| "invalid provider metadata")?;
            if parsed.is_object() {
                Ok(parsed)
            } else {
                Err("invalid provider metadata")
            }
        }
        _ => Err("invalid provider metadata"),
    }
}

fn gemini_base_url(url: &str) -> Result<&'static str, &'static str> {
    match url {
        "https://generativelanguage.googleapis.com/v1beta"
        | "https://generativelanguage.googleapis.com/v1beta/openai" => {
            Ok("https://generativelanguage.googleapis.com/v1beta/openai")
        }
        _ => Err("unsupported Gemini base URL; use Google's OpenAI-compatible endpoint"),
    }
}

fn horde_base_url(url: &str) -> Result<&'static str, &'static str> {
    match url {
        "https://aihorde.net/api/v2" | "https://oai.aihorde.net/v1" => {
            Ok("https://oai.aihorde.net/v1")
        }
        _ => Err("unsupported AI Horde base URL; use its official OpenAI-compatible endpoint"),
    }
}

pub(super) fn resolve(
    connection: &Value,
    nodes: &[Value],
    keyed: bool,
    warnings: &mut Vec<String>,
) -> Result<(String, String), &'static str> {
    let id = string(connection, "provider");
    let node = nodes.iter().find(|node| string(node, "id") == id);
    if crate::devin::bridge(id) {
        return Ok((DEVIN_ACP_KIND.into(), DEVIN_ACP_BASE_URL.into()));
    }
    let (kind, fallback) = if let Some((_, kind, url)) = PROVIDERS.iter().find(|(p, _, _)| *p == id)
    {
        (*kind, *url)
    } else {
        let node = node.ok_or("unknown/non-compatible provider or custom protocol settings")?;
        let kind = match string(node, "type") {
            "openai-compatible"
                if matches!(string(node, "apiType"), "" | "chat" | "chat-completions") =>
            {
                KIND_OPENAI_COMPAT
            }
            "anthropic-compatible" => KIND_ANTHROPIC,
            _ => return Err("unknown/non-compatible provider or custom protocol settings"),
        };
        if !string(node, "chatPath").is_empty() {
            return Err("unknown/non-compatible provider or custom protocol settings");
        }
        (kind, string(node, "baseUrl"))
    };
    let metadata = metadata(connection)?;
    let override_url = string(&metadata, "baseUrl");
    let url = if override_url.is_empty() {
        fallback
    } else {
        override_url
    };
    let url = url.trim_end_matches('/');
    if url.is_empty() {
        return Err("missing upstream base URL");
    }
    let suffix = match kind {
        KIND_ANTHROPIC => "/messages",
        KIND_OPENAI_COMPAT | llmux_types::provider::OPENAI => "/chat/completions",
        _ => "",
    };
    let url = url.strip_suffix(suffix).unwrap_or(url);
    let provider = ProviderIdentity::connection(id, nodes);
    if kind == KIND_DEEPAI && url != DEEPAI_BASE {
        return Err("DeepAI requires its official text API base URL");
    }
    if kind == KIND_OPENAI_COMPAT
        && url == DEEPAI_CODE_SIDECAR
        && (provider.is_builtin("deepai-code")
            || node.is_some_and(|node| {
                string(node, "prefix") == "deepai" && string(node, "type") == "openai-compatible"
            }))
    {
        if !keyed {
            return Err("DeepAI Code sidecar requires an API key");
        }
        warnings.push(format!(
            "connection {}: reviewed DeepAI Code sidecar must be reachable on the llmux2 host",
            string(connection, "id")
        ));
        return Ok((kind.into(), url.into()));
    }
    let identity = super::connection::identity(connection, nodes);
    let url = if provider.is_builtin("morph") && local_host(url, false) {
        warnings.push(format!("connection {}: reviewed Morph loopback override replaced with official https://api.morphllm.com/v1", string(connection, "id")));
        super::morph::OFFICIAL_URL
    } else if local_host(url, true)
        && !(local_host(url, false) && super::connection::keyed_loopback_bridge(identity))
    {
        return Err("loopback/localhost upstream; configure a reachable connection base URL");
    } else {
        url
    };
    let url = if id == "gemini" {
        gemini_base_url(url)?
    } else if id == "aihorde" {
        horde_base_url(url)?
    } else {
        url
    };
    if id == "morph" && url != "https://api.morphllm.com/v1" {
        return Err("Morph requires its public official Morph endpoint");
    }
    if matches!(
        super::connection::identity(connection, nodes),
        "kilo" | "kilocode"
    ) && url != "https://ai-gateway.kilo.ai/api/v1"
    {
        return Err("Kilo requires its public official gateway endpoint");
    }
    Ok((kind.into(), url.into()))
}
