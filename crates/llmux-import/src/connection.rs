use crate::{string, valid_secret};
use serde_json::Value;

pub(super) struct SelectedKey<'a> {
    pub value: &'a str,
    pub anonymous: bool,
}

fn identity<'a>(connection: &'a Value, nodes: &'a [Value]) -> &'a str {
    let provider = string(connection, "provider");
    nodes
        .iter()
        .find(|node| string(node, "id") == provider && !string(node, "prefix").is_empty())
        .map(|node| string(node, "prefix"))
        .unwrap_or(provider)
}

fn replacement(provider: &str) -> Option<&'static str> {
    match provider {
        "cloudflare-direct" | "cloudflare-playground" => Some(
            "replace with a Cloudflare Workers AI account API token at https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1 and a free-eligible model",
        ),
        "duckduckgo-web" | "deepai" | "deepai-code" => Some("no official replacement; dropped"),
        "hms" => Some("custom headers unsupported in llmux2"),
        _ => None,
    }
}

pub(super) fn select_key<'a>(
    connection: &'a Value,
    nodes: &[Value],
) -> Result<SelectedKey<'a>, &'static str> {
    if connection["usable"] == false {
        return Err("usable: false");
    }
    if connection["isActive"] == false {
        return Err("inactive");
    }
    let provider = identity(connection, nodes);
    if let Some(reason) = replacement(provider) {
        return Err(reason);
    }
    let auth = string(connection, "authType");
    if matches!(auth, "no-auth" | "none") {
        if provider == "aihorde" && string(connection, "provider") == "aihorde" {
            return Ok(SelectedKey {
                value: "0000000000",
                anonymous: true,
            });
        }
        if matches!(provider, "kilo" | "kilocode") {
            return Err("anonymous access needs requests with no auth header, which llmux2 can't configure; add a Kilo API key");
        }
    }
    if !matches!(auth, "apikey" | "api_key" | "api-key") {
        return Err("OAuth/token or unsupported auth");
    }
    let value = connection["apiKey"].as_str().unwrap_or("");
    if !valid_secret(value) {
        return Err("missing, encrypted or invalid API key");
    }
    Ok(SelectedKey {
        value,
        anonymous: false,
    })
}
