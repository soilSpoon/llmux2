use crate::{array, string, valid_secret};
use serde_json::Value;

#[derive(Clone, Copy)]
pub(super) enum SelectedAuth<'a> {
    Key(&'a str),
    None,
    Workers,
}

pub(super) fn valid_restrictions(step: &Value) -> bool {
    !step
        .get("connectionId")
        .is_some_and(|v| !v.is_null() && !v.is_string())
        && !step
            .get("allowedConnectionIds")
            .is_some_and(|v| !v.is_null() && !v.is_array())
        && array(step, "allowedConnectionIds")
            .iter()
            .all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()))
}

pub(super) fn identity<'a>(connection: &'a Value, nodes: &'a [Value]) -> &'a str {
    let provider = string(connection, "provider");
    nodes
        .iter()
        .find(|node| string(node, "id") == provider && !string(node, "prefix").is_empty())
        .map(|node| string(node, "prefix"))
        .unwrap_or(provider)
}

fn replacement(provider: &str) -> Option<&'static str> {
    match provider {
        "cloudflare-direct" | "cloudflare-playground" | "cfpd" | "cfp" => Some(
            "replace with a Cloudflare Workers AI account API token at https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1 and a free-eligible model",
        ),
        "duckduckgo-web" | "deepai" | "deepai-code" => Some("no official replacement; dropped"),
        _ => None,
    }
}

pub(super) fn select_auth<'a>(
    connection: &'a Value,
    nodes: &[Value],
    workers: bool,
) -> Result<SelectedAuth<'a>, &'static str> {
    if connection["usable"] == false {
        return Err("usable: false");
    }
    if connection["isActive"] == false {
        return Err("inactive");
    }
    let provider = identity(connection, nodes);
    if workers && crate::policy::cloudflare(provider) {
        if matches!(
            string(connection, "authType"),
            "apikey" | "api_key" | "api-key" | "none" | "no-auth"
        ) {
            return Ok(SelectedAuth::Workers);
        }
        return Err("OAuth/token or unsupported auth");
    }
    if let Some(reason) = replacement(provider) {
        return Err(reason);
    }
    if crate::devin::bridge(provider) {
        return Ok(SelectedAuth::None);
    }
    let auth = string(connection, "authType");
    if matches!(auth, "no-auth" | "none") {
        if nodes
            .iter()
            .any(|node| string(node, "id") == string(connection, "provider"))
        {
            return Ok(SelectedAuth::None);
        }
        if provider == "aihorde" && string(connection, "provider") == "aihorde" {
            return Ok(SelectedAuth::None);
        }
        if matches!(provider, "kilo" | "kilocode") {
            return Ok(SelectedAuth::None);
        }
    }
    if !matches!(auth, "apikey" | "api_key" | "api-key") {
        return Err("OAuth/token or unsupported auth");
    }
    let value = connection["apiKey"].as_str().unwrap_or("");
    if !valid_secret(value) {
        return Err("missing, encrypted or invalid API key");
    }
    Ok(SelectedAuth::Key(value))
}
