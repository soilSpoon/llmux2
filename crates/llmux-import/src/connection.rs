use crate::{array, string, valid_secret};
use serde_json::Value;

pub(super) struct OauthImport {
    pub refresh: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub project: Option<String>,
    pub workspace: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub kind: &'static str,
    pub base_url: &'static str,
    pub token_url: &'static str,
}

pub(super) enum SelectedAuth<'a> {
    Key(&'a str),
    None,
    Workers,
    Oauth(OauthImport),
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
        "cloudflare-playground" | "cfp" | "duckduckgo-web" | "ddgw" => Some(SESSION_FALLBACK),
        _ => None,
    }
}

pub(super) const SESSION_FALLBACK: &str = "no native adapter; pass --omniroute-fallback and --omniroute-key to keep serving this provider through OmniRoute";

pub(super) fn session_provider(provider: &str) -> bool {
    matches!(
        provider,
        "duckduckgo-web" | "ddgw" | "cloudflare-playground" | "cfp"
    )
}

fn workers_hint() -> &'static str {
    "replace with a Cloudflare Workers AI account API token at https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1 and a free-eligible model"
}

fn owned_field(connection: &Value, metadata: &Value, names: &[&str]) -> String {
    for name in names {
        let value = string(connection, name);
        if !value.is_empty() {
            return value.to_owned();
        }
        let value = string(metadata, name);
        if !value.is_empty() {
            return value.to_owned();
        }
    }
    String::new()
}

fn oauth_target(provider: &str) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match provider {
        "xai-oauth" | "xao" => (
            "xai",
            "https://api.x.ai/v1",
            "https://auth.x.ai/oauth2/token",
        ),
        "codex" | "cx" => (
            "openai_responses",
            "https://chatgpt.com/backend-api/codex",
            "https://auth.openai.com/oauth/token",
        ),
        "antigravity" => (
            "antigravity",
            "https://daily-cloudcode-pa.googleapis.com",
            "https://oauth2.googleapis.com/token",
        ),
        _ => return None,
    })
}

fn scopes(raw: &str) -> Option<Vec<String>> {
    let scopes: Vec<String> = raw
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect();
    (!scopes.is_empty()).then_some(scopes)
}

fn oauth(connection: &Value, provider: &str) -> Result<OauthImport, &'static str> {
    let Some((kind, base_url, token_url)) = oauth_target(provider) else {
        return Err("OAuth/token or unsupported auth");
    };
    let metadata = crate::upstream::metadata(connection).unwrap_or_else(|_| serde_json::json!({}));
    let refresh = owned_field(connection, &metadata, &["refreshToken", "refresh_token"]);
    if refresh.is_empty() {
        return Err("missing refresh token");
    }
    if !valid_secret(&refresh) {
        return Err("encrypted or invalid refresh token");
    }
    let client_id = owned_field(connection, &metadata, &["clientId", "client_id"]);
    if client_id.is_empty() || !valid_secret(&client_id) {
        return Err("missing oauth client id");
    }
    let secret = owned_field(connection, &metadata, &["clientSecret", "client_secret"]);
    let client_secret = if secret.is_empty() {
        None
    } else if valid_secret(&secret) {
        Some(secret)
    } else {
        return Err("encrypted or invalid oauth client secret");
    };
    let project = owned_field(
        connection,
        &metadata,
        &["projectId", "project_id", "project"],
    );
    if kind == "antigravity" && (project.is_empty() || !valid_secret(&project)) {
        return Err("missing Antigravity project id");
    }
    let workspace = owned_field(
        connection,
        &metadata,
        &["workspaceId", "workspace_id", "chatgptAccountId"],
    );
    let scope = owned_field(connection, &metadata, &["scope", "scopes"]);
    Ok(OauthImport {
        refresh,
        client_id,
        client_secret,
        project: (!project.is_empty()).then_some(project),
        workspace: (!workspace.is_empty() && valid_secret(&workspace)).then_some(workspace),
        scopes: scopes(&scope),
        kind,
        base_url,
        token_url,
    })
}

fn bridge_base_url(connection: &Value, nodes: &[Value]) -> bool {
    let metadata = crate::upstream::metadata(connection).unwrap_or_else(|_| serde_json::json!({}));
    if !string(&metadata, "baseUrl").is_empty() {
        return true;
    }
    nodes.iter().any(|node| {
        string(node, "id") == string(connection, "provider") && !string(node, "baseUrl").is_empty()
    })
}

fn keyed_bridge(connection: &Value, nodes: &[Value]) -> bool {
    matches!(
        string(connection, "authType"),
        "apikey" | "api_key" | "api-key"
    ) && valid_secret(connection["apiKey"].as_str().unwrap_or(""))
        && bridge_base_url(connection, nodes)
}

pub(super) fn keyed_loopback_bridge(provider: &str) -> bool {
    matches!(provider, "cloudflare-direct" | "cfpd")
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
    if matches!(provider, "cloudflare-direct" | "cfpd") && !keyed_bridge(connection, nodes) {
        return Err(workers_hint());
    }

    if crate::devin::bridge(provider) {
        return Ok(SelectedAuth::None);
    }
    let auth = string(connection, "authType");
    if matches!(auth, "oauth" | "oauth2") {
        let raw = string(connection, "provider");
        let target = if oauth_target(provider).is_some() {
            provider
        } else {
            raw
        };
        return oauth(connection, target).map(SelectedAuth::Oauth);
    }
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
