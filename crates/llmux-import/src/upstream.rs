use crate::{string, PROVIDERS};
use http::Uri;
use llmux_types::provider::{ANTHROPIC as KIND_ANTHROPIC, OPENAI_COMPAT as KIND_OPENAI_COMPAT};
use serde_json::{json, Value};
use std::net::IpAddr;

pub(super) const PROVIDER_ALIASES: &[(&str, &str)] = &[("ds", "deepseek"), ("kc", "kilocode")];

pub(super) fn canonical_provider<'a>(provider: &'a str, nodes: &'a [Value]) -> &'a str {
    if let Some((_, canonical)) = PROVIDER_ALIASES
        .iter()
        .find(|(alias, _)| *alias == provider)
    {
        return canonical;
    }
    nodes
        .iter()
        .find(|node| string(node, "prefix") == provider)
        .map(|node| string(node, "id"))
        .unwrap_or(provider)
}

pub(super) fn model_id<'a>(raw: &'a str, provider: &str, nodes: &[Value]) -> &'a str {
    let node = nodes.iter().find(|node| string(node, "id") == provider);
    let mut model = raw;
    loop {
        let Some((segment, rest)) = model.split_once('/') else {
            return model;
        };
        if segment == provider
            || PROVIDER_ALIASES
                .iter()
                .any(|(alias, id)| *id == provider && *alias == segment)
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

pub(super) fn model_target<'a>(step: &'a Value, nodes: &'a [Value]) -> Option<(&'a str, &'a str)> {
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
        resolved
    };
    let model = if resolved.is_empty()
        && (!provider_id.is_empty() || nodes.iter().any(|node| string(node, "id") == canonical))
    {
        model_id(model, canonical, nodes)
    } else {
        model
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

fn local_host(url: &str) -> bool {
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
        Ok(IpAddr::V4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|ip| ip.is_loopback() || ip.is_unspecified())
        }
        Err(_) => false,
    }
}

fn metadata(connection: &Value) -> Result<Value, &'static str> {
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
) -> Result<(String, String), &'static str> {
    let id = string(connection, "provider");
    let (kind, fallback) = if let Some((_, kind, url)) = PROVIDERS.iter().find(|(p, _, _)| *p == id)
    {
        (*kind, *url)
    } else {
        let node = nodes
            .iter()
            .find(|node| string(node, "id") == id)
            .ok_or("unknown/non-compatible provider or custom protocol settings")?;
        let kind = match string(node, "type") {
            "openai-compatible"
                if matches!(string(node, "apiType"), "" | "chat" | "chat-completions") =>
            {
                KIND_OPENAI_COMPAT
            }
            "anthropic-compatible" => KIND_ANTHROPIC,
            _ => return Err("unknown/non-compatible provider or custom protocol settings"),
        };
        if !string(node, "chatPath").is_empty()
            || node["customHeaders"]
                .as_object()
                .is_some_and(|headers| !headers.is_empty())
        {
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
    if local_host(url) {
        return Err("loopback/localhost upstream; configure a reachable connection base URL");
    }
    let suffix = if kind == KIND_ANTHROPIC {
        "/messages"
    } else {
        "/chat/completions"
    };
    let url = url.strip_suffix(suffix).unwrap_or(url);
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
