//! Parse and validate an immutable MVP routing configuration.

use http::Uri;
use llmux_types::{
    Billing, CanonicalModel, Capabilities, ContextOverflow, Deployment, DeploymentIdx, ErrorKind,
    ProviderKind, QuotaSpec, Strategy,
};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 86_400_000;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read configuration: {0}")]
    Read(#[source] std::io::Error),
    #[error("invalid configuration syntax or field type at byte {offset}")]
    Parse { offset: usize },
    #[error("{0}")]
    Invalid(String),
}

pub struct ServerConfig {
    pub listen: SocketAddr,
    pub client_keys: Box<[Arc<str>]>,
    pub request_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub drain_grace: Duration,
    pub expose_routing_headers: bool,
    pub dashboard_key: Option<Arc<str>>,
}

pub struct RoutingConfig {
    pub max_attempts: usize,
    pub fallback_on: Box<[ErrorKind]>,
}

pub struct CooldownConfig {
    pub default: Duration,
    pub max: Duration,
}

pub struct ApiKey {
    pub provider: ProviderKind,
    pub key: Arc<str>,
}

pub struct RuntimeConfig {
    pub server: ServerConfig,
    pub routing: RoutingConfig,
    pub cooldown: CooldownConfig,
    pub api_keys: HashMap<Arc<str>, ApiKey>,
    pub oauth: HashMap<Arc<str>, OAuthCredential>,
    pub model_names: HashMap<Arc<str>, usize>,
    pub models: Box<[CanonicalModel]>,
    pub deployments: Box<[Deployment]>,
}

pub struct OAuthCredential {
    pub token_url: String,
    pub client_id: String,
    pub refresh_token: Arc<str>,
    pub client_secret: Option<Arc<str>>,
    pub scopes: Option<Vec<String>>,
    pub resource: Option<String>,
    pub refresh_token_path: Option<PathBuf>,
    pub refresh_skew: Duration,
    pub request_timeout: Duration,
}

impl RuntimeConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path).map_err(ConfigError::Read)?;
        Self::parse(&content)
    }

    pub fn parse(content: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(content).map_err(|err| ConfigError::Parse {
            offset: err.span().map_or(0, |span| span.start),
        })?;
        raw.into_runtime()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    server: RawServer,
    #[serde(default)]
    routing: RawRouting,
    #[serde(default)]
    cooldown: RawCooldown,
    #[serde(default)]
    credentials: Vec<RawCredential>,
    models: Vec<RawModel>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawServer {
    listen: SocketAddr,
    client_keys: Vec<String>,
    request_timeout_ms: u64,
    shutdown_timeout_ms: u64,
    drain_grace_ms: u64,
    expose_routing_headers: bool,
    dashboard_key: Option<String>,
}

impl Default for RawServer {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
            client_keys: Vec::new(),
            request_timeout_ms: DEFAULT_TIMEOUT_MS,
            shutdown_timeout_ms: 60_000,
            drain_grace_ms: 2_000,
            expose_routing_headers: true,
            dashboard_key: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawRouting {
    default_strategy: Strategy,
    max_attempts: usize,
    retry_same_deployment: bool,
    fallback_on: Vec<ErrorKind>,
}

impl Default for RawRouting {
    fn default() -> Self {
        Self {
            default_strategy: Strategy::Priority,
            max_attempts: 4,
            retry_same_deployment: false,
            fallback_on: vec![ErrorKind::RateLimited, ErrorKind::QuotaExhausted],
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawCooldown {
    default_ms: u64,
    max_ms: u64,
}

impl Default for RawCooldown {
    fn default() -> Self {
        Self {
            default_ms: 30_000,
            max_ms: 3_600_000,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
enum RawCredential {
    ApiKey {
        id: String,
        provider: String,
        key: String,
    },
    Oauth {
        id: String,
        provider: String,
        token_url: Option<String>,
        client_id: Option<String>,
        refresh_token: Option<String>,
        client_secret: Option<String>,
        scopes: Option<Vec<String>>,
        resource: Option<String>,
        refresh_skew_ms: Option<u64>,
        request_timeout_ms: Option<u64>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    strategy: Option<Strategy>,
    #[serde(default)]
    capabilities: Capabilities,
    #[serde(default)]
    context_overflow: ContextOverflow,
    deployments: Vec<RawDeployment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDeployment {
    id: String,
    credential: Option<String>,
    auth: Option<DeploymentAuth>,
    provider: Option<String>,
    base_url: String,
    upstream_model: String,
    #[serde(default)]
    priority: i32,
    weight: Option<u32>,
    #[serde(default)]
    billing: Billing,
    #[serde(default)]
    quota: QuotaSpec,
    timeout_ms: Option<u64>,
    max_context: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeploymentAuth {
    None,
}

impl RawConfig {
    fn into_runtime(self) -> Result<RuntimeConfig, ConfigError> {
        let RawConfig {
            server,
            routing,
            cooldown,
            credentials,
            models,
        } = self;
        let request_timeout = timeout(server.request_timeout_ms)?;
        if server.drain_grace_ms > 10_000 {
            return Err(invalid("server.drain_grace_ms must be between 0 and 10000"));
        }
        if server.client_keys.is_empty() {
            return Err(invalid(
                "server.client_keys must contain at least one secret reference",
            ));
        }
        let client_keys = server
            .client_keys
            .iter()
            .map(|key| resolve_secret(key))
            .collect::<Result<Box<[_]>, _>>()?;
        let dashboard_key = server
            .dashboard_key
            .as_deref()
            .map(resolve_secret)
            .transpose()?;
        if dashboard_key
            .as_ref()
            .is_some_and(|key| client_keys.contains(key))
        {
            return Err(invalid(
                "server.dashboard_key must differ from server.client_keys",
            ));
        }
        if routing.max_attempts == 0 {
            return Err(invalid("routing.max_attempts must be positive"));
        }
        if routing.default_strategy != Strategy::Priority
            || routing.retry_same_deployment
            || routing
                .fallback_on
                .iter()
                .any(|kind| !matches!(kind, ErrorKind::RateLimited | ErrorKind::QuotaExhausted))
        {
            return Err(invalid("MVP routing supports priority, no same-deployment retries, and rate_limited or quota_exhausted fallback only"));
        }
        if cooldown.default_ms == 0
            || cooldown.max_ms < cooldown.default_ms
            || cooldown.max_ms > MAX_TIMEOUT_MS
        {
            return Err(invalid(
                "cooldown must satisfy 0 < default_ms <= max_ms <= 86400000",
            ));
        }

        let mut provider_kinds = HashMap::new();
        let mut api_keys = HashMap::new();
        let mut oauth = HashMap::new();
        for credential in credentials {
            let (id, provider) = match &credential {
                RawCredential::ApiKey { id, provider, .. }
                | RawCredential::Oauth { id, provider, .. } => (id, provider),
            };
            if id.is_empty() || provider.is_empty() || provider_kinds.contains_key(id.as_str()) {
                return Err(invalid(
                    "credential IDs must be unique and IDs and providers must be nonempty",
                ));
            }
            let id: Arc<str> = id.as_str().into();
            let provider = ProviderKind(provider.as_str().into());
            provider_kinds.insert(id.clone(), provider.clone());
            match credential {
                RawCredential::ApiKey { key, .. } => {
                    api_keys.insert(
                        id,
                        ApiKey {
                            provider,
                            key: resolve_secret(&key)?,
                        },
                    );
                }
                RawCredential::Oauth {
                    token_url,
                    client_id,
                    refresh_token,
                    client_secret,
                    scopes,
                    resource,
                    refresh_skew_ms,
                    request_timeout_ms,
                    ..
                } => {
                    let token_url = required_oauth(token_url, "token_url")?;
                    upstream_uri(&token_url).map_err(|_| invalid("oauth token_url must be HTTPS (HTTP only for loopback), without credentials, query or fragment"))?;
                    let client_id = required_oauth(client_id, "client_id")?;
                    let reference = required_oauth(refresh_token, "refresh_token")?;
                    let refresh_token = resolve_secret(&reference)?;
                    let refresh_token_path = reference.strip_prefix("file:").map(PathBuf::from);
                    if let Some(resource) = &resource {
                        let valid = resource.parse::<Uri>().is_ok_and(|uri| {
                            uri.scheme_str() == Some("https")
                                && uri.authority().is_some_and(|a| {
                                    !a.host().is_empty() && !a.as_str().contains('@')
                                })
                                && !resource.contains('#')
                        });
                        if !valid {
                            return Err(invalid("oauth resource must be an absolute HTTPS URL without credentials or fragment"));
                        }
                    }
                    let client_secret = client_secret.as_deref().map(resolve_secret).transpose()?;
                    let scopes = scopes.filter(|scopes| !scopes.is_empty());
                    if scopes.as_ref().is_some_and(|scopes| {
                        scopes.iter().any(|scope| {
                            scope.is_empty()
                                || !scope.bytes().all(|b| {
                                    b == 0x21
                                        || (0x23..=0x5b).contains(&b)
                                        || (0x5d..=0x7e).contains(&b)
                                })
                        })
                    }) {
                        return Err(invalid(
                            "oauth scopes must contain nonempty OAuth scope tokens",
                        ));
                    }
                    let refresh_skew_ms = refresh_skew_ms.unwrap_or(60_000);
                    if refresh_skew_ms > MAX_TIMEOUT_MS {
                        return Err(invalid("oauth refresh_skew_ms must be at most 86400000"));
                    }
                    oauth.insert(
                        id,
                        OAuthCredential {
                            token_url,
                            client_id,
                            refresh_token,
                            client_secret,
                            scopes,
                            resource,
                            refresh_token_path,
                            refresh_skew: Duration::from_millis(refresh_skew_ms),
                            request_timeout: timeout(request_timeout_ms.unwrap_or(30_000))?,
                        },
                    );
                }
            }
        }

        if models.is_empty() {
            return Err(invalid("at least one model is required"));
        }
        let mut model_names = HashMap::new();
        let mut canonical_models = Vec::new();
        let mut deployment_ids = HashSet::new();
        let mut deployments = Vec::new();
        for model in models {
            let name: Arc<str> = model.name.into();
            for alias in
                std::iter::once(name.clone()).chain(model.aliases.into_iter().map(Arc::from))
            {
                if alias.is_empty() || model_names.insert(alias, canonical_models.len()).is_some() {
                    return Err(invalid(
                        "model names and aliases must be nonempty and globally unique",
                    ));
                }
            }
            let strategy = model.strategy.unwrap_or(routing.default_strategy);
            if strategy != Strategy::Priority || model.deployments.is_empty() {
                return Err(invalid(
                    "each model needs at least one deployment and must use priority routing",
                ));
            }

            if model.capabilities.max_context == Some(0) {
                return Err(invalid("max_context must be positive"));
            }
            let mut candidates = Vec::new();
            for dep in model.deployments {
                let id: Arc<str> = dep.id.into();
                if !valid_identifier(&id) || !deployment_ids.insert(id.clone()) {
                    return Err(invalid("deployment IDs must be unique and use ASCII letters, digits, '.', '_' or '-'"));
                }
                let provider = match (&dep.auth, &dep.credential, &dep.provider) {
                    (Some(DeploymentAuth::None), None, Some(provider))
                        if matches!(provider.as_str(), "openai" | "openai_compat") =>
                    {
                        ProviderKind(provider.as_str().into())
                    }
                    (None, Some(credential), None) => provider_kinds
                        .get(credential.as_str())
                        .cloned()
                        .ok_or_else(|| invalid("deployment references an unknown credential"))?,
                    _ => return Err(invalid("deployment requires a credential, or auth = 'none' with an OpenAI-compatible provider; do not combine them")),
                };
                if dep.upstream_model.is_empty() || dep.weight == Some(0) {
                    return Err(invalid(
                        "upstream_model must not be empty and weight must be positive",
                    ));
                }
                if dep.max_context == Some(0) {
                    return Err(invalid("max_context must be positive"));
                }
                if dep.quota.rpm == Some(0) || dep.quota.concurrent == Some(0) {
                    return Err(invalid("configured limits must be positive"));
                }
                if dep.quota.rpd.is_some() || dep.quota.tpm.is_some() || dep.quota.tpd.is_some() {
                    return Err(invalid("MVP quotas support rpm and concurrent only"));
                }
                let idx = DeploymentIdx(
                    u32::try_from(deployments.len())
                        .map_err(|_| invalid("too many deployments"))?,
                );
                candidates.push(idx);
                deployments.push(Deployment {
                    idx,
                    id,
                    provider,
                    credential: dep.credential.unwrap_or_default().into(),
                    base_url: upstream_uri(&dep.base_url)?,
                    upstream_model: dep.upstream_model.into(),
                    priority: dep.priority,
                    weight: dep.weight.unwrap_or(1),
                    billing: dep.billing,
                    quota: dep.quota,
                    timeout: timeout(dep.timeout_ms.unwrap_or(server.request_timeout_ms))?,
                    max_context: dep.max_context.or(model.capabilities.max_context),
                });
            }

            candidates.sort_by_key(|idx| deployments[idx.0 as usize].priority);
            canonical_models.push(CanonicalModel {
                name,
                strategy,
                capabilities: model.capabilities,
                context_overflow: model.context_overflow,
                deployments: candidates.into_boxed_slice(),
            });
        }

        Ok(RuntimeConfig {
            server: ServerConfig {
                listen: server.listen,
                client_keys,
                request_timeout,
                shutdown_timeout: timeout(server.shutdown_timeout_ms)?,
                drain_grace: Duration::from_millis(server.drain_grace_ms),
                expose_routing_headers: server.expose_routing_headers,
                dashboard_key,
            },
            routing: RoutingConfig {
                max_attempts: routing.max_attempts,
                fallback_on: routing.fallback_on.into_boxed_slice(),
            },
            cooldown: CooldownConfig {
                default: Duration::from_millis(cooldown.default_ms),
                max: Duration::from_millis(cooldown.max_ms),
            },
            api_keys,
            oauth,
            model_names,
            models: canonical_models.into_boxed_slice(),
            deployments: deployments.into_boxed_slice(),
        })
    }
}

fn invalid(message: &str) -> ConfigError {
    ConfigError::Invalid(message.to_owned())
}

fn required_oauth(value: Option<String>, field: &str) -> Result<String, ConfigError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid(&format!("oauth {field} is required and must be nonempty")))
}

fn valid_identifier(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn timeout(ms: u64) -> Result<Duration, ConfigError> {
    if !(1..=MAX_TIMEOUT_MS).contains(&ms) {
        return Err(invalid("timeouts must be 1 through 86400000 milliseconds"));
    }
    Ok(Duration::from_millis(ms))
}

fn upstream_uri(value: &str) -> Result<Uri, ConfigError> {
    if let Ok(uri) = value.parse::<Uri>() {
        if let (Some(authority), Some(scheme)) = (uri.authority(), uri.scheme_str()) {
            let loopback = authority
                .host()
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
                || authority.host().eq_ignore_ascii_case("localhost");
            if (scheme == "https" || (scheme == "http" && loopback))
                && !authority.as_str().contains('@')
                && uri.query().is_none()
                && !value.contains('#')
            {
                return Ok(uri);
            }
        }
    }
    Err(invalid("base_url must be HTTPS (HTTP is allowed only for loopback) with no credentials, query or fragment"))
}

fn resolve_secret(reference: &str) -> Result<Arc<str>, ConfigError> {
    let value = if let Some(name) = reference
        .strip_prefix("env:")
        .filter(|name| !name.is_empty())
    {
        std::env::var(name)
            .map_err(|_| invalid("could not read a secret from its environment reference"))?
    } else if let Some(path) = reference
        .strip_prefix("file:")
        .filter(|path| !path.is_empty())
    {
        fs::read_to_string(path)
            .map_err(|_| invalid("could not read a secret from its file reference"))?
    } else {
        return Err(invalid(
            "secrets must be env: or file: references, not literal values",
        ));
    };
    let val = value.trim_end_matches(['\r', '\n']);
    if val.is_empty() || !val.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(invalid(
            "secrets must be nonempty, printable ASCII without whitespace",
        ));
    }
    Ok(Arc::from(val))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_auth_requires_explicit_unambiguous_opt_in() {
        let _ = self::config("");
        let prefix = "[server]\nclient_keys = [\"env:LLMUX_CONFIG_OAUTH_TEST_SECRET\"]\n";
        let config = |auth: &str| {
            format!("{prefix}\n[[models]]\nname = \"test\"\n[[models.deployments]]\nid = \"anonymous\"\nbase_url = \"https://example.com/v1\"\nupstream_model = \"test\"\n{auth}\n")
        };
        assert!(super::RuntimeConfig::parse(&config(
            "auth = \"none\"\nprovider = \"openai_compat\""
        ))
        .is_ok());
        for auth in [
            "",
            "provider = \"openai_compat\"",
            "auth = \"none\"",
            "auth = \"none\"\nprovider = \"anthropic\"",
            "auth = \"typo\"\nprovider = \"openai_compat\"",
            "auth = \"none\"\nprovider = \"openai_compat\"\ncredential = \"missing\"",
            "credential = \"missing\"",
        ] {
            assert!(
                super::RuntimeConfig::parse(&config(auth)).is_err(),
                "{auth}"
            );
        }
    }

    fn config(fields: &str) -> String {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| std::env::set_var("LLMUX_CONFIG_OAUTH_TEST_SECRET", "test-secret"));
        format!(
            r#"
[server]
client_keys = ["env:LLMUX_CONFIG_OAUTH_TEST_SECRET"]
[[credentials]]
id = "oauth"
kind = "oauth"
provider = "openai_compat"
{fields}
[[models]]
name = "model"
[[models.deployments]]
id = "deployment"
credential = "oauth"
base_url = "https://example.com/v1"
upstream_model = "upstream"
"#
        )
    }

    const REQUIRED: &str = r#"token_url = "https://example.com/token"
client_id = "client"
refresh_token = "env:LLMUX_CONFIG_OAUTH_TEST_SECRET""#;

    #[test]
    fn shutdown_timeout_validation() {
        let base = config(REQUIRED);
        assert_eq!(
            RuntimeConfig::parse(&base).unwrap().server.shutdown_timeout,
            Duration::from_secs(60)
        );
        for value in [0, 86_400_001] {
            let input = base.replace(
                "[server]",
                &format!("[server]\nshutdown_timeout_ms = {value}"),
            );
            assert!(RuntimeConfig::parse(&input)
                .err()
                .unwrap()
                .to_string()
                .contains("timeouts"));
        }
        assert!(RuntimeConfig::parse(
            &base.replace("[server]", "[server]\nshutdown_timeout_ms = 1234")
        )
        .is_ok());
    }

    #[test]
    fn drain_grace_validation() {
        let base = config(REQUIRED);
        assert_eq!(
            RuntimeConfig::parse(&base).unwrap().server.drain_grace,
            Duration::from_millis(2000)
        );
        for value in [0, 1, 2000, 10_000] {
            let input = base.replace("[server]", &format!("[server]\ndrain_grace_ms = {value}"));
            assert_eq!(
                RuntimeConfig::parse(&input).unwrap().server.drain_grace,
                Duration::from_millis(value)
            );
        }
        let input = base.replace("[server]", "[server]\ndrain_grace_ms = 10001");
        assert!(RuntimeConfig::parse(&input)
            .err()
            .unwrap()
            .to_string()
            .contains("drain_grace_ms"));
    }

    fn error(fields: &str) -> String {
        RuntimeConfig::parse(&config(fields))
            .err()
            .expect("invalid config")
            .to_string()
    }

    #[test]
    fn oauth_defaults_and_options() {
        let parsed = RuntimeConfig::parse(&config(REQUIRED)).unwrap();
        let credential = &parsed.oauth["oauth"];
        assert!(parsed.api_keys.is_empty());
        assert_eq!(credential.token_url, "https://example.com/token");
        assert_eq!(credential.client_id, "client");
        assert_eq!(&*credential.refresh_token, "test-secret");
        assert!(credential.client_secret.is_none());
        assert!(credential.scopes.is_none());
        assert_eq!(credential.refresh_skew, Duration::from_secs(60));
        assert_eq!(credential.request_timeout, Duration::from_secs(30));
        let parsed = RuntimeConfig::parse(&config(&format!("{REQUIRED}\nclient_secret = \"env:LLMUX_CONFIG_OAUTH_TEST_SECRET\"\nscopes = [\"read\", \"write\"]\nrefresh_skew_ms = 0\nrequest_timeout_ms = 1000"))).unwrap();
        let credential = &parsed.oauth["oauth"];
        assert_eq!(credential.client_secret.as_deref(), Some("test-secret"));
        assert_eq!(credential.scopes.as_deref().unwrap(), ["read", "write"]);
        assert_eq!(credential.refresh_skew, Duration::ZERO);
        assert_eq!(credential.request_timeout, Duration::from_secs(1));
    }

    #[test]
    fn oauth_resource_validation() {
        let parsed = RuntimeConfig::parse(&config(REQUIRED)).unwrap();
        assert!(parsed.oauth["oauth"].resource.is_none());
        assert!(parsed.oauth["oauth"].refresh_token_path.is_none());
        let parsed = RuntimeConfig::parse(&config(&format!(
            "{REQUIRED}\nresource = \"https://api.openai.com/v1\""
        )))
        .unwrap();
        assert_eq!(
            parsed.oauth["oauth"].resource.as_deref(),
            Some("https://api.openai.com/v1")
        );
        for resource in [
            "",
            "relative/path",
            "http://api.openai.com/v1",
            "https:///",
            "https://user:secret@example.com/v1",
            "https://example.com/v1#fragment",
        ] {
            assert!(error(&format!("{REQUIRED}\nresource = {resource:?}")).contains("resource"));
        }
    }

    #[test]
    fn oauth_empty_scopes_are_absent() {
        let parsed = RuntimeConfig::parse(&config(&format!("{REQUIRED}\nscopes = []"))).unwrap();
        assert!(parsed.oauth["oauth"].scopes.is_none());
    }

    #[test]
    fn oauth_required_fields_are_clear_and_nonempty() {
        for field in ["token_url", "client_id", "refresh_token"] {
            let fields = REQUIRED
                .lines()
                .filter(|line| !line.starts_with(field))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(error(&fields).contains(&format!("oauth {field} is required")));
            assert!(error(&format!("{fields}\n{field} = \"\""))
                .contains(&format!("oauth {field} is required")));
        }
    }

    #[test]
    fn oauth_rejects_invalid_endpoint_secrets_and_options() {
        for url in [
            "http://example.com/token",
            "not-a-url",
            "https://user:secret@example.com/token",
        ] {
            assert!(error(&REQUIRED.replace("https://example.com/token", url))
                .contains("oauth token_url"));
        }
        assert!(
            error(&REQUIRED.replace("env:LLMUX_CONFIG_OAUTH_TEST_SECRET", "inline-secret"))
                .contains("not literal values")
        );
        assert!(
            error(&format!("{REQUIRED}\nclient_secret = \"inline-secret\""))
                .contains("not literal values")
        );
        assert!(error(&format!("{REQUIRED}\nscopes = [\"\"]")).contains("oauth scopes"));
        assert!(error(&format!("{REQUIRED}\nrequest_timeout_ms = 0")).contains("timeouts"));
        assert!(error(&format!("{REQUIRED}\nrefresh_skew_ms = 86400001"))
            .contains("oauth refresh_skew_ms"));
    }
    fn context_config(model: &str, deployment: &str) -> String {
        config(REQUIRED).replace(
            "[[models]]\nname = \"model\"\n",
            &format!("[[models]]\nname = \"model\"\n{model}\n"),
        ) + &format!(
            "{deployment}\n[[models.deployments]]\nid = \"other\"\ncredential = \"oauth\"\nbase_url = \"https://example.com/v1\"\nupstream_model = \"upstream\"\n"
        )
    }

    #[test]
    fn context_overflow_defaults_to_reject() {
        let parsed = RuntimeConfig::parse(&context_config("", "")).unwrap();
        assert_eq!(parsed.models[0].context_overflow, ContextOverflow::Reject);
        let parsed =
            RuntimeConfig::parse(&context_config(r#"context_overflow = "middle-out""#, ""))
                .unwrap();
        assert_eq!(
            parsed.models[0].context_overflow,
            ContextOverflow::MiddleOut
        );
        assert!(
            RuntimeConfig::parse(&context_config(r#"context_overflow = "truncate""#, "")).is_err()
        );
    }

    #[test]
    fn deployment_max_context_overrides_model_capability() {
        let parsed = RuntimeConfig::parse(&context_config("", "")).unwrap();
        assert!(parsed
            .deployments
            .iter()
            .all(|dep| dep.max_context.is_none()));
        let parsed = RuntimeConfig::parse(&context_config(
            "capabilities = { max_context = 8192 }",
            "max_context = 32768",
        ))
        .unwrap();
        let limits: Vec<_> = parsed
            .deployments
            .iter()
            .map(|dep| dep.max_context)
            .collect();
        assert_eq!(limits, [Some(32768), Some(8192)]);
        for (model, deployment) in [
            ("capabilities = { max_context = 0 }", ""),
            ("", "max_context = 0"),
        ] {
            assert!(RuntimeConfig::parse(&context_config(model, deployment))
                .err()
                .expect("invalid config")
                .to_string()
                .contains("max_context must be positive"));
        }
    }
}
