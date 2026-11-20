//! Parse and validate an immutable MVP routing configuration.

use http::Uri;
use llmux_types::{
    Billing, CanonicalModel, Capabilities, Deployment, DeploymentIdx, ErrorKind, ProviderKind,
    QuotaSpec, Strategy,
};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    fs,
    net::SocketAddr,
    path::Path,
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
    pub expose_routing_headers: bool,
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
    pub model_names: HashMap<Arc<str>, usize>,
    pub models: Box<[CanonicalModel]>,
    pub deployments: Box<[Deployment]>,
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
    credentials: Vec<RawCredential>,
    models: Vec<RawModel>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawServer {
    listen: SocketAddr,
    client_keys: Vec<String>,
    request_timeout_ms: u64,
    expose_routing_headers: bool,
}

impl Default for RawServer {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
            client_keys: Vec::new(),
            request_timeout_ms: DEFAULT_TIMEOUT_MS,
            expose_routing_headers: true,
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
    deployments: Vec<RawDeployment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDeployment {
    id: String,
    credential: String,
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
        for RawCredential::ApiKey { id, provider, key } in credentials {
            if id.is_empty() || provider.is_empty() || provider_kinds.contains_key(id.as_str()) {
                return Err(invalid(
                    "credential IDs must be unique and IDs and providers must be nonempty",
                ));
            }
            let id: Arc<str> = id.into();
            let provider = ProviderKind(provider.into());
            provider_kinds.insert(id.clone(), provider.clone());
            api_keys.insert(
                id,
                ApiKey {
                    provider,
                    key: resolve_secret(&key)?,
                },
            );
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

            let mut candidates = Vec::new();
            for dep in model.deployments {
                let id: Arc<str> = dep.id.into();
                if !valid_identifier(&id) || !deployment_ids.insert(id.clone()) {
                    return Err(invalid("deployment IDs must be unique and use ASCII letters, digits, '.', '_' or '-'"));
                }
                let provider = provider_kinds
                    .get(dep.credential.as_str())
                    .cloned()
                    .ok_or_else(|| invalid("deployment references an unknown credential"))?;
                if dep.upstream_model.is_empty() || dep.weight == Some(0) {
                    return Err(invalid(
                        "upstream_model must not be empty and weight must be positive",
                    ));
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
                    credential: dep.credential.into(),
                    base_url: upstream_uri(&dep.base_url)?,
                    upstream_model: dep.upstream_model.into(),
                    priority: dep.priority,
                    weight: dep.weight.unwrap_or(1),
                    billing: dep.billing,
                    quota: dep.quota,
                    timeout: timeout(dep.timeout_ms.unwrap_or(server.request_timeout_ms))?,
                });
            }

            candidates.sort_by_key(|idx| deployments[idx.0 as usize].priority);
            canonical_models.push(CanonicalModel {
                name,
                strategy,
                capabilities: model.capabilities,
                deployments: candidates.into_boxed_slice(),
            });
        }

        Ok(RuntimeConfig {
            server: ServerConfig {
                listen: server.listen,
                client_keys,
                request_timeout,
                expose_routing_headers: server.expose_routing_headers,
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
            model_names,
            models: canonical_models.into_boxed_slice(),
            deployments: deployments.into_boxed_slice(),
        })
    }
}

fn invalid(message: &str) -> ConfigError {
    ConfigError::Invalid(message.to_owned())
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
