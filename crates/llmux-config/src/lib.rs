//! Parse and validate an immutable MVP routing configuration.

use http::{HeaderName, Uri};
use llmux_types::{
    provider, AcpAgentSpec, AcpSessionMode, Billing, CanonicalModel, Capabilities,
    ChatRequestField, ContextOverflow, Deployment, DeploymentIdx, ErrorKind, NousState,
    ProviderKind, QuotaSpec,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
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
    /// Model-name policy per `client_keys` entry; `None` leaves the key unrestricted.
    pub key_policies: Box<[Option<KeyPolicy>]>,
    /// Glob patterns matching requested model names no key may use.
    pub deny: Box<[String]>,
    pub request_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub drain_grace: Duration,
    pub expose_routing_headers: bool,
    pub dashboard_key: Option<Arc<str>>,
    /// Append-only JSONL file for per-request routing records.
    pub request_log: Option<PathBuf>,
    /// Inbound header whose value is hashed into the end-user id. `None` disables the feature.
    pub end_user_header: Option<HeaderName>,
    /// Secret mixed into the end-user hash. Present only when `end_user_header` is set.
    pub end_user_salt: Option<Arc<str>>,
    /// When the header is absent, hash the OpenAI `user` body field. Default true if the header is set.
    pub end_user_from_body: bool,
    pub remote_images: RemoteImages,
    /// Decoded byte cap for inline `data:` and Anthropic base64 images. `None` is unlimited.
    pub max_image_bytes: Option<u64>,
}

/// Whether chat requests may carry an http(s) image URL.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RemoteImages {
    #[default]
    Allow,
    Reject,
}

/// Glob allowlist/blocklist restricting which requested model names one
/// gateway client key may use. An empty `models` allows every name;
/// `blocked` matches are then subtracted.
pub struct KeyPolicy {
    pub models: Box<[String]>,
    pub blocked: Box<[String]>,
}

pub struct RoutingConfig {
    pub max_attempts: usize,
    pub fallback_on: Box<[ErrorKind]>,
}

pub struct CooldownConfig {
    pub default: Duration,
    pub max: Duration,
    /// Cap for authoritative quota resets. Provider-reported resets (daily,
    /// monthly) can outlast `max` by days; estimates never do.
    pub quota_max: Duration,
}

pub struct ApiKey {
    pub provider: ProviderKind,
    pub key: Arc<str>,
}

#[derive(Debug)]
pub struct CredentialMeta {
    pub provider: ProviderKind,
    pub oauth: bool,
}

pub struct RuntimeConfig {
    pub server: ServerConfig,
    pub routing: RoutingConfig,
    pub cooldown: CooldownConfig,
    pub api_keys: HashMap<Arc<str>, ApiKey>,
    pub oauth: HashMap<Arc<str>, OAuthCredential>,
    pub nous: HashMap<Arc<str>, NousPortalCredential>,
    /// Agent-CLI process specs keyed by credential id; the `acp_agent` adapter
    /// spawns the declared child per credential.
    pub acp_agents: HashMap<Arc<str>, AcpAgentSpec>,
    /// Quotas shared by every deployment that uses the credential.
    pub credential_quotas: HashMap<Arc<str>, QuotaSpec>,
    pub credential_meta: HashMap<Arc<str>, CredentialMeta>,
    pub model_names: HashMap<Arc<str>, usize>,
    pub catch_all: Option<usize>,
    pub catch_all_exclude: Vec<String>,
    pub model_allowed_keys: HashMap<usize, Box<[Arc<str>]>>,
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

pub struct NousPortalCredential {
    pub state_path: PathBuf,
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
    request_log: Option<PathBuf>,
    end_user_header: Option<String>,
    end_user_salt: Option<String>,
    end_user_from_body: Option<bool>,
    remote_images: RemoteImages,
    max_image_bytes: Option<u64>,
    key_policies: Vec<RawKeyPolicy>,
    deny: Vec<String>,
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
            request_log: None,
            end_user_header: None,
            end_user_salt: None,
            end_user_from_body: None,
            remote_images: RemoteImages::Allow,
            max_image_bytes: None,
            key_policies: Vec::new(),
            deny: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKeyPolicy {
    key: String,
    models: Option<Vec<String>>,
    blocked: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawRouting {
    max_attempts: usize,
    fallback_on: Vec<ErrorKind>,
}

impl Default for RawRouting {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            fallback_on: vec![ErrorKind::RateLimited, ErrorKind::QuotaExhausted],
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawCooldown {
    default_ms: u64,
    max_ms: u64,
    quota_max_ms: u64,
}

impl Default for RawCooldown {
    fn default() -> Self {
        Self {
            default_ms: 30_000,
            max_ms: 3_600_000,
            // 32 days — beyond any plausible daily or monthly reset horizon.
            quota_max_ms: 2_764_800_000,
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
        #[serde(default)]
        quota: QuotaSpec,
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
        #[serde(default)]
        quota: QuotaSpec,
    },
    NousPortal {
        id: String,
        provider: String,
        state_file: String,
        refresh_skew_ms: Option<u64>,
        request_timeout_ms: Option<u64>,
        #[serde(default)]
        quota: QuotaSpec,
    },
    /// A locally spawned agent CLI the gateway manages as a child and speaks
    /// ACP JSON-RPC to. `provider` must be `acp_agent`.
    AcpAgent {
        id: String,
        provider: String,
        /// The CLI executable: an absolute path (the child's environment is
        /// scrubbed, so there is no PATH to search).
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Extra environment variables; `env:`/`file:` references resolve like
        /// deployment header values, everything else is literal.
        #[serde(default)]
        env: BTreeMap<String, String>,
        /// Working directory for the child and the ACP session; defaults to `home`.
        cwd: Option<String>,
        /// Sandbox home: the child's HOME and XDG dirs point here so the CLI
        /// reads its own config and credentials from this directory.
        home: String,
        turn_timeout_ms: Option<u64>,
        session_mode: Option<RawAcpSessionMode>,
        /// ACP `authenticate` method id called after `initialize`, for agents
        /// that require authentication before `session/new` (Antigravity's
        /// `agy acp` uses `oauth-personal`).
        auth_method: Option<String>,
        #[serde(default)]
        quota: QuotaSpec,
    },
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawAcpSessionMode {
    PerRequest,
    Persistent,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    name: String,
    exclude: Option<Vec<String>>,
    allowed_keys: Option<Vec<String>>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    capabilities: Capabilities,
    #[serde(default)]
    context_overflow: ContextOverflow,
    deployments: Vec<RawDeployment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDeployment {
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    fallback: bool,
    id: String,
    credential: Option<String>,
    auth: Option<DeploymentAuth>,
    provider: Option<String>,
    base_url: Option<String>,
    upstream_model: String,
    #[serde(default)]
    strip_request_fields: Box<[ChatRequestField]>,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    quota: QuotaSpec,
    timeout_ms: Option<u64>,
    max_context: Option<u32>,
    expires_at: Option<toml::value::Datetime>,
    priority_boost: Option<i32>,
    priority_boost_until: Option<toml::value::Datetime>,
    #[serde(default)]
    billing: Billing,
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
        let end_user = end_user_settings(&server)?;
        let key_policies = resolve_key_policies(server.key_policies, &client_keys)?;
        if server.deny.iter().any(|pattern| pattern.is_empty()) {
            return Err(invalid("server.deny patterns must be nonempty"));
        }
        let deny = server.deny;
        let max_image_bytes = match server.max_image_bytes {
            Some(0) => return Err(invalid("max_image_bytes must be positive")),
            other => other,
        };
        if routing.max_attempts == 0 {
            return Err(invalid("routing.max_attempts must be positive"));
        }
        if routing
            .fallback_on
            .iter()
            .any(|kind| !matches!(kind, ErrorKind::RateLimited | ErrorKind::QuotaExhausted))
        {
            return Err(invalid(
                "routing.fallback_on supports rate_limited and quota_exhausted only",
            ));
        }
        if cooldown.default_ms == 0
            || cooldown.max_ms < cooldown.default_ms
            || cooldown.max_ms > MAX_TIMEOUT_MS
            || cooldown.quota_max_ms < cooldown.max_ms
        {
            return Err(invalid(
                "cooldown must satisfy 0 < default_ms <= max_ms <= 86400000 and quota_max_ms >= max_ms",
            ));
        }

        let mut provider_kinds = HashMap::new();
        let mut credential_meta = HashMap::new();
        let mut api_keys = HashMap::new();
        let mut oauth = HashMap::new();
        let mut nous = HashMap::new();
        let mut acp_agents = HashMap::new();
        let mut credential_quotas = HashMap::new();
        for credential in credentials {
            let (id, provider, quota) = match &credential {
                RawCredential::ApiKey {
                    id,
                    provider,
                    quota,
                    ..
                }
                | RawCredential::Oauth {
                    id,
                    provider,
                    quota,
                    ..
                }
                | RawCredential::NousPortal {
                    id,
                    provider,
                    quota,
                    ..
                }
                | RawCredential::AcpAgent {
                    id,
                    provider,
                    quota,
                    ..
                } => (id, provider, *quota),
            };
            if quota.concurrent.is_some()
                || [quota.rpm, quota.rpd, quota.tpm, quota.tpd].contains(&Some(0))
            {
                return Err(invalid(
                    "credential quota supports positive rpm, rpd, tpm and tpd only",
                ));
            }
            if id.is_empty() || provider.is_empty() || provider_kinds.contains_key(id.as_str()) {
                return Err(invalid(
                    "credential IDs must be unique and IDs and providers must be nonempty",
                ));
            }
            let id: Arc<str> = id.as_str().into();
            let provider = ProviderKind(provider.as_str().into());
            provider_kinds.insert(id.clone(), provider.clone());
            credential_meta.insert(
                id.clone(),
                CredentialMeta {
                    provider: provider.clone(),
                    oauth: matches!(
                        credential,
                        RawCredential::Oauth { .. } | RawCredential::NousPortal { .. }
                    ),
                },
            );
            if quota != QuotaSpec::default() {
                credential_quotas.insert(id.clone(), quota);
            }
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
                RawCredential::AcpAgent {
                    command,
                    args,
                    env,
                    cwd,
                    home,
                    turn_timeout_ms,
                    session_mode,
                    auth_method,
                    ..
                } => {
                    if provider.0.as_ref() != provider::ACP_AGENT {
                        return Err(invalid(
                            "acp_agent credentials must set provider = \"acp_agent\"",
                        ));
                    }
                    let absolute = |value: &str, field: &str| -> Result<PathBuf, ConfigError> {
                        let path = PathBuf::from(value);
                        if value.is_empty() || !path.is_absolute() {
                            return Err(invalid(&format!(
                                "acp_agent {field} must be an absolute path"
                            )));
                        }
                        Ok(path)
                    };
                    let command = absolute(&command, "command")?;
                    let home = absolute(&home, "home")?;
                    let cwd = cwd.as_deref().map(|cwd| absolute(cwd, "cwd")).transpose()?;
                    let env = env
                        .iter()
                        .map(|(name, value)| {
                            if name.is_empty() || name.bytes().any(|b| b == b'=' || b == 0) {
                                return Err(invalid(
                                    "acp_agent env names must be nonempty and must not contain '='",
                                ));
                            }
                            if name == "HOME" || name.starts_with("XDG_") {
                                return Err(invalid(
                                    "acp_agent env may not override the sandbox HOME or XDG_* dirs",
                                ));
                            }
                            let value = if value.starts_with("env:") || value.starts_with("file:") {
                                resolve_reference(value)?
                            } else {
                                value.clone()
                            };
                            if value.bytes().any(|b| b == 0) {
                                return Err(invalid("acp_agent env values must not contain NUL"));
                            }
                            Ok((name.clone(), value))
                        })
                        .collect::<Result<BTreeMap<_, _>, ConfigError>>()?;
                    let auth_method = auth_method
                        .map(|m| {
                            if m.is_empty() {
                                Err(invalid("acp_agent auth_method must be nonempty"))
                            } else {
                                Ok(m)
                            }
                        })
                        .transpose()?;
                    acp_agents.insert(
                        id,
                        AcpAgentSpec {
                            command,
                            args,
                            env,
                            cwd,
                            home,
                            turn_timeout: timeout(turn_timeout_ms.unwrap_or(120_000))?,
                            mode: match session_mode.unwrap_or(RawAcpSessionMode::PerRequest) {
                                RawAcpSessionMode::PerRequest => AcpSessionMode::PerRequest,
                                RawAcpSessionMode::Persistent => AcpSessionMode::Persistent,
                            },
                            auth_method,
                        },
                    );
                }
                RawCredential::NousPortal {
                    state_file,
                    refresh_skew_ms,
                    request_timeout_ms,
                    ..
                } => {
                    let state_path = PathBuf::from(&state_file);
                    if state_file.is_empty() || !state_path.is_absolute() {
                        return Err(invalid("nous_portal state_file must be an absolute path"));
                    }
                    if NousState::load(&state_path).is_err() {
                        return Err(invalid("nous_portal state_file is missing or unreadable; run `llmux-login nous` to sign in and create it"));
                    }
                    let refresh_skew_ms = refresh_skew_ms.unwrap_or(120_000);
                    if refresh_skew_ms > MAX_TIMEOUT_MS {
                        return Err(invalid(
                            "nous_portal refresh_skew_ms must be at most 86400000",
                        ));
                    }
                    nous.insert(
                        id,
                        NousPortalCredential {
                            state_path,
                            refresh_skew: Duration::from_millis(refresh_skew_ms),
                            request_timeout: timeout(request_timeout_ms.unwrap_or(30_000))?,
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
        let mut catch_all = None;
        let mut catch_all_exclude = Vec::new();
        let mut model_allowed_keys = HashMap::new();
        let mut canonical_models = Vec::new();
        let mut deployment_ids = HashSet::new();
        let mut deployments = Vec::new();
        for model in models {
            let is_catch_all = model.name == "*";
            if is_catch_all {
                if catch_all.replace(canonical_models.len()).is_some()
                    || !model.aliases.is_empty()
                    || model.allowed_keys.is_some()
                {
                    return Err(invalid(
                        "only one catch-all is allowed, without aliases or allowed_keys",
                    ));
                }
                for pattern in model.exclude.unwrap_or_default() {
                    if pattern.is_empty()
                        || pattern.strip_suffix('*').unwrap_or(&pattern).contains('*')
                    {
                        return Err(invalid("exclude entries must be nonempty exact ids or prefixes with one trailing *"));
                    }
                    catch_all_exclude.push(pattern.to_ascii_lowercase());
                }
            } else if model.exclude.is_some() {
                return Err(invalid("exclude is only valid on the catch-all model"));
            }
            let allowed_keys = model
                .allowed_keys
                .as_ref()
                .map(|refs| {
                    let keys = refs
                        .iter()
                        .map(|key| resolve_secret(key))
                        .collect::<Result<Box<[_]>, _>>()?;
                    if keys.is_empty() || keys.iter().any(|key| !client_keys.contains(key)) {
                        return Err(invalid(
                            "allowed_keys must be a nonempty subset of server.client_keys",
                        ));
                    }
                    Ok(keys)
                })
                .transpose()?;
            if let Some(keys) = allowed_keys {
                model_allowed_keys.insert(canonical_models.len(), keys);
            }
            let name: Arc<str> = model.name.into();
            for alias in std::iter::once(name.clone())
                .chain(model.aliases.into_iter().map(Arc::from))
                .filter(|_| !is_catch_all)
            {
                if alias.is_empty()
                    || alias.as_ref() == "*"
                    || model_names.insert(alias, canonical_models.len()).is_some()
                {
                    return Err(invalid(
                        "model names and aliases must be nonempty and globally unique",
                    ));
                }
            }
            if model.deployments.is_empty() {
                return Err(invalid("each model needs at least one deployment"));
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
                        if matches!(
                            provider.as_str(),
                            provider::OPENAI
                                | provider::OPENAI_COMPAT
                                | provider::DEVIN_ACP
                                | provider::AWS_QUICK
                        ) =>
                    {
                        ProviderKind(provider.as_str().into())
                    }
                    (None, Some(credential), None) => provider_kinds
                        .get(credential.as_str())
                        .cloned()
                        .ok_or_else(|| invalid("deployment references an unknown credential"))?,
                    _ => return Err(invalid("deployment requires a credential, or auth = 'none' with an OpenAI-compatible provider; do not combine them")),
                };
                if provider.0.as_ref() == provider::OPENAI_RESPONSES
                    && dep
                        .credential
                        .as_deref()
                        .is_some_and(|id| oauth.contains_key(id))
                    && !model_allowed_keys.contains_key(&canonical_models.len())
                {
                    return Err(invalid("OAuth openai_responses models require allowed_keys for the owner's gateway keys"));
                }
                if !dep.strip_request_fields.is_empty() {
                    match provider.0.as_ref() {
                        provider::OPENAI | provider::OPENAI_COMPAT => {}
                        provider::ANTHROPIC
                            if dep.strip_request_fields.iter().all(|field| {
                                matches!(
                                    field,
                                    ChatRequestField::Temperature
                                        | ChatRequestField::TopP
                                        | ChatRequestField::TopK
                                )
                            }) => {}
                        provider::ANTHROPIC => {
                            return Err(invalid(
                                "strip_request_fields on anthropic accepts only temperature, top_p and top_k",
                            ))
                        }
                        _ => {
                            return Err(invalid(
                                "strip_request_fields requires openai, openai_compat or anthropic",
                            ))
                        }
                    }
                }
                let base_url = dep
                    .base_url
                    .as_deref()
                    .or_else(|| {
                        (provider.0.as_ref() == provider::OPENAI_RESPONSES)
                            .then_some("https://api.openai.com/v1")
                    })
                    .or_else(|| {
                        (provider.0.as_ref() == provider::DEVIN_ACP)
                            .then_some(provider::DEVIN_ACP_BASE_URL)
                    })
                    .or_else(|| {
                        (provider.0.as_ref() == provider::ACP_AGENT)
                            .then_some(provider::ACP_AGENT_BASE_URL)
                    })
                    .or_else(|| {
                        (provider.0.as_ref() == provider::AWS_QUICK)
                            .then_some(provider::AWS_QUICK_BASE_URL)
                    })
                    .ok_or_else(|| invalid("deployment requires base_url"))?;
                if dep.upstream_model.is_empty() {
                    return Err(invalid("upstream_model must not be empty"));
                }
                if (dep.upstream_model == "*") != is_catch_all {
                    return Err(invalid(
                        "upstream_model must be * exactly for catch-all deployments",
                    ));
                }
                if dep.max_context == Some(0) {
                    return Err(invalid("max_context must be positive"));
                }
                let quota = dep.quota;
                if [quota.rpm, quota.rpd, quota.tpm, quota.tpd, quota.concurrent].contains(&Some(0))
                {
                    return Err(invalid("configured limits must be positive"));
                }
                let idx = DeploymentIdx(
                    u32::try_from(deployments.len())
                        .map_err(|_| invalid("too many deployments"))?,
                );
                candidates.push(idx);
                let expires_at = dep.expires_at.map(|stamp| expiry(&id, stamp)).transpose()?;
                let priority_boost_until = dep
                    .priority_boost_until
                    .map(|stamp| boost_until(&id, stamp))
                    .transpose()?;
                if dep.priority_boost.is_some() && priority_boost_until.is_none() {
                    return Err(invalid(&format!(
                        "deployment {id} priority_boost requires priority_boost_until"
                    )));
                }
                deployments.push(Deployment {
                    headers: deployment_headers(
                        dep.headers,
                        dep.auth.is_some(),
                        provider.0.as_ref(),
                    )?,
                    fallback: dep.fallback || is_catch_all,
                    idx,
                    id,
                    provider,
                    credential: dep.credential.unwrap_or_default().into(),
                    base_url: upstream_uri(base_url)?,
                    upstream_model: dep.upstream_model.into(),
                    strip_request_fields: dep.strip_request_fields,
                    priority: dep.priority,
                    quota: dep.quota,
                    timeout: timeout(dep.timeout_ms.unwrap_or(server.request_timeout_ms))?,
                    max_context: dep.max_context.or(model.capabilities.max_context),
                    expires_at,
                    priority_boost: dep.priority_boost,
                    priority_boost_until,
                    billing: dep.billing,
                });
            }

            candidates.sort_by_key(|idx| deployments[idx.0 as usize].priority);
            canonical_models.push(CanonicalModel {
                name,
                capabilities: model.capabilities,
                context_overflow: model.context_overflow,
                deployments: candidates.into_boxed_slice(),
            });
        }

        Ok(RuntimeConfig {
            credential_quotas,
            server: ServerConfig {
                listen: server.listen,
                client_keys,
                key_policies,
                deny: deny.into_boxed_slice(),
                request_timeout,
                shutdown_timeout: timeout(server.shutdown_timeout_ms)?,
                drain_grace: Duration::from_millis(server.drain_grace_ms),
                expose_routing_headers: server.expose_routing_headers,
                dashboard_key,
                request_log: server.request_log,
                end_user_header: end_user.header,
                end_user_salt: end_user.salt,
                end_user_from_body: end_user.from_body,
                remote_images: server.remote_images,
                max_image_bytes,
            },
            routing: RoutingConfig {
                max_attempts: routing.max_attempts,
                fallback_on: routing.fallback_on.into_boxed_slice(),
            },
            cooldown: CooldownConfig {
                default: Duration::from_millis(cooldown.default_ms),
                max: Duration::from_millis(cooldown.max_ms),
                quota_max: Duration::from_millis(cooldown.quota_max_ms),
            },
            api_keys,
            oauth,
            nous,
            acp_agents,
            credential_meta,
            model_names,
            catch_all,
            catch_all_exclude,
            model_allowed_keys,
            models: canonical_models.into_boxed_slice(),
            deployments: deployments.into_boxed_slice(),
        })
    }
}

fn invalid(message: &str) -> ConfigError {
    ConfigError::Invalid(message.to_owned())
}

/// Resolve `[[server.key_policies]]` entries into a `client_keys`-aligned
/// vector. Each entry must name one configured key; an entry that neither
/// allows nor blocks anything is meaningless and rejected.
fn resolve_key_policies(
    raw: Vec<RawKeyPolicy>,
    client_keys: &[Arc<str>],
) -> Result<Box<[Option<KeyPolicy>]>, ConfigError> {
    let mut policies: Vec<Option<KeyPolicy>> = (0..client_keys.len()).map(|_| None).collect();
    for entry in raw {
        let resolved = resolve_secret(&entry.key)?;
        let Some(index) = client_keys
            .iter()
            .position(|key| key.as_ref() == resolved.as_ref())
        else {
            return Err(invalid(
                "server.key_policies keys must be entries of server.client_keys",
            ));
        };
        let models = entry.models.unwrap_or_default();
        let blocked = entry.blocked.unwrap_or_default();
        if models.is_empty() && blocked.is_empty() {
            return Err(invalid(
                "server.key_policies entries need a nonempty models or blocked list",
            ));
        }
        if models
            .iter()
            .chain(blocked.iter())
            .any(|pattern| pattern.is_empty())
        {
            return Err(invalid("server.key_policies patterns must be nonempty"));
        }
        if policies[index]
            .replace(KeyPolicy {
                models: models.into(),
                blocked: blocked.into(),
            })
            .is_some()
        {
            return Err(invalid("server.key_policies must not repeat a key"));
        }
    }
    Ok(policies.into_boxed_slice())
}

struct EndUserSettings {
    header: Option<HeaderName>,
    salt: Option<Arc<str>>,
    from_body: bool,
}

fn end_user_settings(server: &RawServer) -> Result<EndUserSettings, ConfigError> {
    let header = match server.end_user_header.as_deref() {
        None => None,
        Some(name) if name.is_empty() || name.trim().is_empty() => {
            return Err(invalid(
                "end_user_header must be a nonempty HTTP header name",
            ));
        }
        Some(name) => Some(end_user_header_name(name)?),
    };
    let salt = match (&header, server.end_user_salt.as_deref()) {
        (None, None) => None,
        (None, Some(_)) => return Err(invalid("end_user_salt requires end_user_header")),
        (Some(_), None | Some("")) => {
            return Err(invalid(
                "end_user_salt is required when end_user_header is set and must be a nonempty env: or file: secret",
            ));
        }
        (Some(_), Some(reference)) => Some(resolve_secret(reference).map_err(|err| {
            invalid(&format!(
                "end_user_salt must be a nonempty env: or file: secret ({err})"
            ))
        })?),
    };
    let from_body = match (header.is_some(), server.end_user_from_body) {
        (false, Some(_)) => return Err(invalid("end_user_from_body requires end_user_header")),
        (false, None) => false,
        (true, None) => true,
        (true, Some(value)) => value,
    };
    Ok(EndUserSettings {
        header,
        salt,
        from_body,
    })
}

fn end_user_header_name(name: &str) -> Result<HeaderName, ConfigError> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| invalid("end_user_header must be a valid HTTP header name"))?;
    if matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "x-api-key"
            | "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
    ) {
        return Err(invalid(
            "end_user_header cannot be a credential, cookie, or hop-by-hop header",
        ));
    }
    Ok(name)
}

fn expiry(deployment: &str, stamp: toml::value::Datetime) -> Result<SystemTime, ConfigError> {
    rfc3339(deployment, "expires_at", stamp)
}

fn boost_until(deployment: &str, stamp: toml::value::Datetime) -> Result<SystemTime, ConfigError> {
    rfc3339(deployment, "priority_boost_until", stamp)
}

fn rfc3339(
    deployment: &str,
    field: &str,
    stamp: toml::value::Datetime,
) -> Result<SystemTime, ConfigError> {
    let text = stamp.to_string();
    let parsed = time::OffsetDateTime::parse(&text, &time::format_description::well_known::Rfc3339)
        .map_err(|_| {
            invalid(&format!(
                "deployment {deployment} {field} must be an RFC 3339 timestamp"
            ))
        })?;
    let seconds: u64 = parsed
        .unix_timestamp()
        .try_into()
        .map_err(|_| invalid(&format!("deployment {deployment} {field} is out of range")))?;
    Ok(SystemTime::UNIX_EPOCH + Duration::new(seconds, parsed.nanosecond()))
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
        if let (Some(authority), Some(_)) = (uri.authority(), uri.scheme_str()) {
            if llmux_types::is_https_or_loopback_http(&uri)
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

fn deployment_headers(
    raw: HashMap<String, String>,
    anonymous: bool,
    provider: &str,
) -> Result<http::HeaderMap, ConfigError> {
    let mut headers = http::HeaderMap::new();
    for (name, reference) in raw {
        let value = if reference.starts_with("env:") || reference.starts_with("file:") {
            resolve_reference(&reference)?
        } else {
            reference
        };
        let (name, value) = validate_deployment_header(&name, &value, anonymous, provider)?;
        if headers.insert(name, value).is_some() {
            return Err(invalid("duplicate deployment header"));
        }
    }
    Ok(headers)
}

pub fn validate_deployment_header(
    name: &str,
    value: &str,
    anonymous: bool,
    provider: &str,
) -> Result<(http::HeaderName, http::HeaderValue), ConfigError> {
    let name = http::HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| invalid("invalid deployment header name"))?;
    if matches!(
        name.as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "upgrade"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "proxy-connection"
            | "http2-settings"
    ) || (!anonymous
        && (name == http::header::AUTHORIZATION
            || (provider == provider::ANTHROPIC && name == provider::ANTHROPIC_KEY_HEADER)
            || (provider == provider::DEEPAI && name == provider::DEEPAI_KEY_HEADER)))
    {
        return Err(invalid("forbidden deployment header"));
    }
    let mut value = http::HeaderValue::from_str(value)
        .map_err(|_| invalid("invalid deployment header value"))?;
    value.set_sensitive(true);
    Ok((name, value))
}

fn resolve_secret(reference: &str) -> Result<Arc<str>, ConfigError> {
    let value = resolve_reference(reference)?;
    let val = value.as_str();
    if val.is_empty() || !val.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(invalid(
            "secrets must be nonempty, printable ASCII without whitespace",
        ));
    }
    Ok(Arc::from(val))
}

fn resolve_reference(reference: &str) -> Result<String, ConfigError> {
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
    Ok(val.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_headers_resolve_and_reject_unsafe_values() {
        let base = config(REQUIRED);
        let loaded = RuntimeConfig::parse(&format!(
            "{base}\nheaders = {{ x-test = \"env:LLMUX_CONFIG_OAUTH_TEST_SECRET\" }}\n"
        ));
        assert!(loaded.is_ok(), "custom headers must load");
        let loaded = loaded.unwrap();
        assert_eq!(loaded.deployments[0].headers["x-test"], "test-secret");
        assert!(loaded.deployments[0].headers["x-test"].is_sensitive());
        assert!(!format!("{:?}", loaded.deployments[0]).contains("test-secret"));
        assert!(RuntimeConfig::parse(&format!("{base}\nheaders = {{}}\n")).is_ok());
        assert!(
            validate_deployment_header("x-api-key", "value", false, provider::ANTHROPIC).is_err()
        );
        assert!(validate_deployment_header("Api-Key", "value", false, provider::DEEPAI).is_err());
        assert!(validate_deployment_header(
            "authorization",
            "value",
            true,
            provider::OPENAI_COMPAT
        )
        .is_ok());
        for name in [
            "Host",
            "Content-Length",
            "Transfer-Encoding",
            "Connection",
            "Keep-Alive",
            "TE",
            "Trailer",
            "Upgrade",
            "Proxy-Authorization",
            "Proxy-Connection",
            "HTTP2-Settings",
            "Authorization",
            "bad name",
        ] {
            assert!(
                RuntimeConfig::parse(&format!("{base}\nheaders = {{ \"{name}\" = \"value\" }}\n"))
                    .is_err(),
                "{name}"
            );
        }
        assert!(RuntimeConfig::parse(&format!(
            "{base}\nheaders = {{ x-test = \"bad\\r\\nvalue\" }}\n"
        ))
        .is_err());
        for reference in [
            "env:LLMUX_MISSING_CUSTOM_HEADER_TEST",
            "file:/nonexistent/llmux-header",
        ] {
            let error = RuntimeConfig::parse(&format!(
                "{base}\nheaders = {{ x-test = \"{reference}\" }}\n"
            ))
            .err()
            .unwrap();
            assert!(!error.to_string().contains(reference));
        }
    }

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

    #[test]
    fn shipped_examples_keep_the_free_offers_out_of_the_quickstart() {
        for (name, value) in [
            ("LLMUX_CLIENT_KEY", "example-client-key"),
            ("GROQ_API_KEY", "example-groq-key"),
            ("TOGETHER_API_KEY", "example-together-key"),
            ("ANTHROPIC_API_KEY", "example-anthropic-key"),
        ] {
            std::env::set_var(name, value);
        }
        let quickstart = RuntimeConfig::parse(include_str!("../../../examples/llmux.example.toml"))
            .expect("the quickstart example must load without the free-tier provider keys");
        assert!(
            quickstart
                .deployments
                .iter()
                .all(|deployment| deployment.expires_at.is_none()),
            "the quickstart example must not carry the opt-in free offers"
        );

        for (name, value) in [
            ("PACKET_API_KEY", "example-packet-key"),
            ("NVIDIA_API_KEY", "example-nvidia-key"),
            ("OPENCODE_ZEN_API_KEY", "example-zen-key"),
            ("FREEMODEL_API_KEY", "example-freemodel-key"),
        ] {
            std::env::set_var(name, value);
        }
        let example = include_str!("../../../examples/llmux.free-providers.example.toml");
        let parsed =
            RuntimeConfig::parse(example).expect("the shipped free-provider example must load");
        let expiry = |seconds: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        let mut seen = HashSet::new();
        for dep in parsed.deployments.iter() {
            seen.insert(dep.id.to_string());
            match dep.id.as_ref() {
                "packet-deepseek-v4.1-flash" | "packet-minimax-m3.1-flash" => {
                    assert_eq!(dep.expires_at, Some(expiry(1_797_465_599)), "{}", dep.id)
                }
                "zen-hy4-flash" => assert_eq!(dep.expires_at, Some(expiry(1_796_083_199))),
                "zen-deepseek-v4.1-flash" | "zen-big-pickle" => {
                    assert_eq!(dep.expires_at, None, "{}", dep.id)
                }
                "nvidia-gemma-4-31b" => {
                    assert_eq!(dep.expires_at, None);
                    assert_eq!(dep.quota.rpm, Some(20));
                }
                "freemodel-claude-fable-5" => {
                    assert_eq!(dep.expires_at, None);
                    assert_eq!(dep.quota.rpm, Some(10));
                    assert_eq!(dep.base_url.to_string(), "https://freemodel.in/v1");
                }
                _ => {}
            }
        }
        for id in [
            "packet-deepseek-v4.1-flash",
            "packet-minimax-m3.1-flash",
            "nvidia-gemma-4-31b",
            "zen-hy4-flash",
            "zen-deepseek-v4.1-flash",
            "zen-big-pickle",
            "freemodel-claude-fable-5",
        ] {
            assert!(seen.contains(id), "example config is missing {id}");
        }
    }

    #[test]
    fn priority_boost_parses_and_requires_a_deadline() {
        let base = config(REQUIRED);
        let parsed = RuntimeConfig::parse(&base.replace(
            "upstream_model = \"upstream\"",
            "priority_boost = -1\npriority_boost_until = 2030-01-01T00:00:00Z\nupstream_model = \"upstream\"",
        ))
        .expect("boost with deadline");
        assert_eq!(parsed.deployments[0].priority_boost, Some(-1));
        assert!(parsed.deployments[0].priority_boost_until.is_some());
        // A promotion without its deadline is meaningless; an unparseable
        // deadline is rejected the same way expires_at is.
        for bad in [
            "priority_boost = -1\nupstream_model = \"upstream\"",
            "priority_boost_until = \"soon\"\nupstream_model = \"upstream\"",
        ] {
            assert!(
                RuntimeConfig::parse(&base.replace("upstream_model = \"upstream\"", bad)).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn deployment_expiry_parses_and_bad_timestamps_are_rejected() {
        let base = config(REQUIRED);
        let parsed = RuntimeConfig::parse(&base.replace(
            "upstream_model = \"upstream\"",
            "expires_at = 2030-01-01T00:00:00Z\nupstream_model = \"upstream\"",
        ))
        .expect("RFC 3339 expiry");
        assert!(parsed.deployments[0].expires_at.is_some());
        let fractional = RuntimeConfig::parse(&base.replace(
            "upstream_model = \"upstream\"",
            "expires_at = 2030-01-01T00:00:00.900Z\nupstream_model = \"upstream\"",
        ))
        .expect("fractional RFC 3339 expiry");
        assert_eq!(
            fractional.deployments[0]
                .expires_at
                .unwrap()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .subsec_millis(),
            900
        );
        assert!(RuntimeConfig::parse(&base).unwrap().deployments[0]
            .expires_at
            .is_none());
        for value in ["\"soon\"", "2030-01-01", "\"2030-01-01 00:00:00\""] {
            assert!(
                RuntimeConfig::parse(&base.replace(
                    "upstream_model = \"upstream\"",
                    &format!("expires_at = {value}\nupstream_model = \"upstream\""),
                ))
                .is_err(),
                "accepted expires_at = {value}"
            );
        }
    }

    #[test]
    fn token_and_daily_quotas_are_accepted_and_must_be_positive() {
        let with_quota = |quota: &str| format!("{}quota = {{ {quota} }}\n", config(REQUIRED));
        RuntimeConfig::parse(&with_quota("rpm = 20, rpd = 50, tpm = 30000, tpd = 200000"))
            .expect("token and daily quotas");
        for quota in ["rpd = 0", "tpm = 0", "tpd = 0"] {
            let err = RuntimeConfig::parse(&with_quota(quota)).err().expect(quota);
            assert!(err.to_string().contains("positive"), "{quota}: {err}");
        }
    }

    #[test]
    fn deployment_billing_parses_and_defaults_to_billed() {
        let base = config(REQUIRED);
        let parsed = RuntimeConfig::parse(&base).unwrap();
        assert_eq!(parsed.deployments[0].billing, Billing::Billed);
        let free = RuntimeConfig::parse(
            &base.replace("upstream_model =", "billing = \"free\"\nupstream_model ="),
        )
        .unwrap();
        assert_eq!(free.deployments[0].billing, Billing::Free);
        for invalid in ["billing = \"paid\"", "billing = \"none\""] {
            assert!(RuntimeConfig::parse(
                &base.replace("upstream_model =", &format!("{invalid}\nupstream_model ="))
            )
            .is_err());
        }
    }

    #[test]
    fn removed_routing_and_deployment_knobs_are_rejected() {
        let base = config(REQUIRED);
        for legacy in [
            "[routing]\ndefault_strategy = \"priority\"\n",
            "[routing]\nretry_same_deployment = true\n",
            "[models]\nstrategy = \"priority\"\n[[models]",
            "weight = 2\n",
            "billing = \"free\"\n",
        ] {
            let input = base.replace("[[models]]", &format!("{legacy}[[models]]"));
            assert!(
                RuntimeConfig::parse(&input).is_err(),
                "legacy key still accepted: {legacy}"
            );
        }
    }

    #[test]
    fn credential_quotas_are_shared_limits_that_must_be_positive() {
        RuntimeConfig::parse(&config(&format!(
            "{REQUIRED}\nquota = {{ rpm = 20, rpd = 50 }}"
        )))
        .expect("credential quota");
        for quota in ["rpm = 0", "concurrent = 2"] {
            let err = RuntimeConfig::parse(&config(&format!("{REQUIRED}\nquota = {{ {quota} }}")))
                .err()
                .expect(quota);
            assert!(
                err.to_string().contains("credential quota"),
                "{quota}: {err}"
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
    fn catch_all_config_validation() {
        let base = config(REQUIRED);
        let wildcard = base
            .replace("name = \"model\"", "name = \"*\"")
            .replace("upstream_model = \"upstream\"", "upstream_model = \"*\"");
        assert!(RuntimeConfig::parse(&wildcard).is_ok());
        assert!(!RuntimeConfig::parse(&base).unwrap().deployments[0].fallback);
        assert!(
            RuntimeConfig::parse(
                &base.replace("upstream_model =", "fallback = true\nupstream_model =")
            )
            .unwrap()
            .deployments[0]
                .fallback
        );
        assert!(RuntimeConfig::parse(&wildcard).unwrap().deployments[0].fallback);
        let excluded = wildcard.replace(
            "name = \"*\"",
            "name = \"*\"\nexclude = [\"codex/*\", \"exact\"]",
        );
        assert!(RuntimeConfig::parse(&excluded).is_ok());
        for input in [
            wildcard.replace("name = \"*\"", "name = \"*\"\naliases = [\"alias\"]"),
            wildcard.replace("name = \"*\"", "name = \"*\"\nallowed_keys = []"),
            wildcard.replace("upstream_model = \"*\"", "upstream_model = \"fixed\""),
            base.replace("upstream_model = \"upstream\"", "upstream_model = \"*\""),
            base.replace("name = \"model\"", "name = \"model\"\nexclude = []"),
            base.replace("name = \"model\"", "name = \"model\"\naliases = [\"*\"]"),
            format!("{wildcard}\n[[models]]\nname = \"*\"\ndeployments = []"),
        ] {
            assert!(RuntimeConfig::parse(&input).is_err(), "accepted {input}");
        }
        for pattern in ["", "a*b", "a**", "*x"] {
            let input = wildcard.replace(
                "name = \"*\"",
                &format!("name = \"*\"\nexclude = [{pattern:?}]"),
            );
            assert!(RuntimeConfig::parse(&input).is_err(), "accepted {pattern}");
        }
    }

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

    #[test]
    fn cooldown_quota_max_defaults_to_32_days_and_must_cover_max_ms() {
        let parsed = RuntimeConfig::parse(&config(REQUIRED)).unwrap();
        assert_eq!(parsed.cooldown.default, Duration::from_secs(30));
        assert_eq!(parsed.cooldown.max, Duration::from_secs(3_600));
        assert_eq!(parsed.cooldown.quota_max, Duration::from_secs(32 * 86_400));
        let with_quota_max = |quota_max_ms: u64| {
            config(REQUIRED).replace(
                "[[credentials]]",
                &format!("[cooldown]\nquota_max_ms = {quota_max_ms}\n[[credentials]]"),
            )
        };
        assert_eq!(
            RuntimeConfig::parse(&with_quota_max(60 * 86_400_000))
                .unwrap()
                .cooldown
                .quota_max,
            Duration::from_secs(60 * 86_400)
        );
        assert_eq!(
            RuntimeConfig::parse(&with_quota_max(3_600_000))
                .unwrap()
                .cooldown
                .quota_max,
            Duration::from_secs(3_600),
            "quota_max_ms may equal max_ms"
        );
        for invalid in [3_599_999, 0] {
            let err = RuntimeConfig::parse(&with_quota_max(invalid))
                .err()
                .expect("quota_max_ms below max_ms is invalid");
            assert!(err.to_string().contains("cooldown"), "{invalid}: {err}");
        }
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

    #[test]
    fn request_log_path_parses_and_defaults_absent() {
        let base = config(REQUIRED);
        let loaded = RuntimeConfig::parse(&base.replace(
            "[server]",
            "[server]\nrequest_log = \"/var/log/llmux/requests.jsonl\"",
        ))
        .unwrap();
        assert_eq!(
            loaded.server.request_log.as_deref(),
            Some(Path::new("/var/log/llmux/requests.jsonl"))
        );
        let loaded = RuntimeConfig::parse(&base).unwrap();
        assert!(loaded.server.request_log.is_none());
    }

    #[test]
    fn strip_request_fields_on_anthropic_accepts_only_sampling_parameters() {
        let base = |provider: &str, fields: &str| {
            std::env::set_var("LLMUX_CONFIG_OAUTH_TEST_SECRET", "test-secret");
            format!(
                r#"
[server]
client_keys = ["env:LLMUX_CONFIG_OAUTH_TEST_SECRET"]
[[credentials]]
id = "key"
kind = "api_key"
provider = "{provider}"
key = "env:LLMUX_CONFIG_OAUTH_TEST_SECRET"
[[models]]
name = "model"
[[models.deployments]]
id = "deployment"
credential = "key"
base_url = "https://example.com/v1"
upstream_model = "upstream"
strip_request_fields = {fields}
"#
            )
        };
        let loaded =
            RuntimeConfig::parse(&base("anthropic", r#"["temperature", "top_p", "top_k"]"#))
                .unwrap();
        assert_eq!(
            loaded.deployments[0].strip_request_fields.as_ref(),
            [
                ChatRequestField::Temperature,
                ChatRequestField::TopP,
                ChatRequestField::TopK
            ]
        );
        assert!(RuntimeConfig::parse(&base("anthropic", r#"["temperature", "seed"]"#)).is_err());
        assert!(RuntimeConfig::parse(&base("xai", r#"["temperature"]"#)).is_err());
        assert!(RuntimeConfig::parse(&base("openai", r#"["seed", "temperature"]"#)).is_ok());
    }

    #[test]
    fn end_user_and_image_policy_config_is_off_by_default_and_rejects_bad_values() {
        std::env::set_var("LLMUX_CONFIG_OAUTH_TEST_SECRET", "test-secret");
        std::env::set_var("LLMUX_END_USER_SALT_TEST", "salt-one");
        std::env::set_var("LLMUX_END_USER_EMPTY_TEST", "");
        let base = r#"
[server]
client_keys = ["env:LLMUX_CONFIG_OAUTH_TEST_SECRET"]
[[credentials]]
id = "key"
kind = "api_key"
provider = "anthropic"
key = "env:LLMUX_CONFIG_OAUTH_TEST_SECRET"
[[models]]
name = "model"
[[models.deployments]]
id = "deployment"
credential = "key"
base_url = "https://example.com/v1"
upstream_model = "upstream"
"#;
        let plain = RuntimeConfig::parse(base).unwrap();
        assert!(plain.server.end_user_header.is_none());
        assert!(plain.server.end_user_salt.is_none());
        assert!(!plain.server.end_user_from_body);
        assert_eq!(plain.server.remote_images, RemoteImages::Allow);
        assert!(plain.server.max_image_bytes.is_none());

        let with = |extra: &str| {
            base.replace(
                "client_keys = [\"env:LLMUX_CONFIG_OAUTH_TEST_SECRET\"]\n",
                &format!("client_keys = [\"env:LLMUX_CONFIG_OAUTH_TEST_SECRET\"]\n{extra}\n"),
            )
        };
        let enabled = RuntimeConfig::parse(&with(
            "end_user_header = \"X-Client-User\"\nend_user_salt = \"env:LLMUX_END_USER_SALT_TEST\"\nremote_images = \"reject\"\nmax_image_bytes = 8",
        ))
        .unwrap();
        assert_eq!(
            enabled.server.end_user_header.unwrap().as_str(),
            "x-client-user"
        );
        assert_eq!(enabled.server.end_user_salt.unwrap().as_ref(), "salt-one");
        assert!(enabled.server.end_user_from_body);
        assert_eq!(enabled.server.remote_images, RemoteImages::Reject);
        assert_eq!(enabled.server.max_image_bytes, Some(8));

        let body_off = RuntimeConfig::parse(&with(
            "end_user_header = \"x-client-user\"\nend_user_salt = \"env:LLMUX_END_USER_SALT_TEST\"\nend_user_from_body = false",
        ))
        .unwrap();
        assert!(!body_off.server.end_user_from_body);

        let literal = "super-secret-salt-value";
        let err = RuntimeConfig::parse(&with(&format!(
            "end_user_header = \"x-client-user\"\nend_user_salt = \"{literal}\""
        )))
        .map(|_| ())
        .unwrap_err();
        assert!(!err.to_string().contains(literal));
        for extra in [
            "end_user_header = \"x-client-user\"",
            "end_user_salt = \"env:LLMUX_END_USER_SALT_TEST\"",
            "end_user_from_body = true",
            "end_user_header = \"\"",
            "end_user_header = \"authorization\"\nend_user_salt = \"env:LLMUX_END_USER_SALT_TEST\"",
            "end_user_header = \"x-client-user\"\nend_user_salt = \"env:LLMUX_END_USER_EMPTY_TEST\"",
            "max_image_bytes = 0",
            "remote_images = \"drop\"",
        ] {
            assert!(RuntimeConfig::parse(&with(extra)).is_err(), "{extra}");
        }

        let path = std::env::temp_dir().join(format!(
            "llmux-end-user-salt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "file-salt\n").unwrap();
        let loaded = RuntimeConfig::parse(&with(&format!(
            "end_user_header = \"x-client-user\"\nend_user_salt = \"file:{}\"",
            path.display()
        )))
        .unwrap();
        assert_eq!(loaded.server.end_user_salt.unwrap().as_ref(), "file-salt");
        std::fs::remove_file(path).unwrap();
    }

    fn acp_config(fields: &str) -> String {
        // nextest runs each test in its own process: the env ref must be
        // populated here, not leaked from another test's fixture.
        std::env::set_var("LLMUX_CONFIG_OAUTH_TEST_SECRET", "test-secret");
        format!(
            r#"
[server]
client_keys = ["env:LLMUX_CONFIG_OAUTH_TEST_SECRET"]
[[credentials]]
id = "agent"
kind = "acp_agent"
provider = "acp_agent"
command = "/usr/bin/env"
args = ["--acp"]
env = {{ AGENT_FLAG = "on", AGENT_KEY = "env:LLMUX_CONFIG_OAUTH_TEST_SECRET" }}
home = "/tmp/agent-home"
{fields}
[[models]]
name = "model"
[[models.deployments]]
id = "deployment"
credential = "agent"
upstream_model = "upstream"
"#
        )
    }

    #[test]
    fn key_policies_parse_align_with_client_keys_and_validate() {
        for (name, value) in [
            ("LLMUX_CONFIG_OAUTH_TEST_SECRET", "test-secret"),
            ("LLMUX_CONFIG_CI_TEST_SECRET", "ci-secret"),
            ("LLMUX_CONFIG_OTHER_TEST_SECRET", "other-secret"),
        ] {
            std::env::set_var(name, value);
        }
        let base = |policies: &str| {
            format!(
                r#"
[server]
client_keys = ["env:LLMUX_CONFIG_OAUTH_TEST_SECRET", "env:LLMUX_CONFIG_CI_TEST_SECRET"]
{policies}
[[credentials]]
id = "key"
kind = "api_key"
provider = "openai_compat"
key = "env:LLMUX_CONFIG_OAUTH_TEST_SECRET"
[[models]]
name = "model"
[[models.deployments]]
id = "deployment"
credential = "key"
base_url = "https://example.com/v1"
upstream_model = "upstream"
"#
            )
        };
        let policy =
            |key: &str, extra: &str| format!("[[server.key_policies]]\nkey = \"{key}\"\n{extra}");
        let ci = "env:LLMUX_CONFIG_CI_TEST_SECRET";

        // Keys without an entry stay unrestricted; the policy aligns by index.
        let parsed = RuntimeConfig::parse(&base("")).unwrap();
        assert!(parsed.server.key_policies.iter().all(Option::is_none));
        let parsed = RuntimeConfig::parse(&base(&policy(
            ci,
            "models = [\"claude-*\", \"gpt-*\"]\nblocked = [\"zc/*\"]",
        )))
        .unwrap();
        assert!(parsed.server.key_policies[0].is_none());
        let entry = parsed.server.key_policies[1].as_ref().unwrap();
        assert_eq!(entry.models.join(","), "claude-*,gpt-*");
        assert_eq!(entry.blocked.join(","), "zc/*");

        // A blocked-only entry is valid: allow-all minus blocked.
        let parsed = RuntimeConfig::parse(&base(&policy(ci, "blocked = [\"zc/*\"]"))).unwrap();
        let entry = parsed.server.key_policies[1].as_ref().unwrap();
        assert!(entry.models.is_empty());
        assert_eq!(entry.blocked.join(","), "zc/*");

        let err = |policies: &str| {
            RuntimeConfig::parse(&base(policies))
                .err()
                .expect("invalid key_policies")
                .to_string()
        };
        assert!(err(&policy(
            "env:LLMUX_CONFIG_OTHER_TEST_SECRET",
            "models = [\"x\"]"
        ))
        .contains("client_keys"));
        assert!(err(&policy("literal-value", "models = [\"x\"]")).contains("env: or file:"));
        for meaningless in [
            "",
            "models = []",
            "blocked = []",
            "models = []\nblocked = []",
        ] {
            assert!(
                err(&policy(ci, meaningless)).contains("models or blocked"),
                "{meaningless}"
            );
        }
        assert!(err(&policy(ci, "models = [\"\"]")).contains("nonempty"));
        assert!(err(&format!(
            "{}\n{}",
            policy(ci, "models = [\"a\"]"),
            policy("env:LLMUX_CONFIG_CI_TEST_SECRET", "blocked = [\"b\"]")
        ))
        .contains("repeat"));
        assert!(RuntimeConfig::parse(&base(&policy(ci, "models = [\"a\"]\nunknown = 1"))).is_err());
    }

    #[test]
    fn acp_agent_credentials_parse_into_process_specs() {
        let loaded = RuntimeConfig::parse(&acp_config(
            "turn_timeout_ms = 5000\nsession_mode = \"persistent\"\ncwd = \"/tmp/agent-work\"",
        ))
        .expect("an acp_agent credential must load");
        let spec = loaded.acp_agents.get("agent").expect("the spec is stored");
        assert_eq!(spec.command, PathBuf::from("/usr/bin/env"));
        assert_eq!(spec.args, vec!["--acp".to_owned()]);
        assert_eq!(spec.env["AGENT_FLAG"], "on");
        assert_eq!(spec.env["AGENT_KEY"], "test-secret");
        assert_eq!(spec.home, PathBuf::from("/tmp/agent-home"));
        assert_eq!(spec.cwd.as_deref(), Some(Path::new("/tmp/agent-work")));
        assert_eq!(spec.turn_timeout, Duration::from_secs(5));
        assert_eq!(spec.mode, AcpSessionMode::Persistent);
        let authed = RuntimeConfig::parse(&acp_config("auth_method = \"oauth-personal\"")).unwrap();
        assert_eq!(
            authed.acp_agents["agent"].auth_method.as_deref(),
            Some("oauth-personal")
        );
        assert!(RuntimeConfig::parse(&acp_config("auth_method = \"\"")).is_err());
        assert_eq!(
            loaded.deployments[0].provider.0.as_ref(),
            provider::ACP_AGENT
        );
        assert_eq!(
            loaded.deployments[0].base_url.to_string(),
            provider::ACP_AGENT_BASE_URL
        );
        assert_eq!(loaded.deployments[0].credential.as_ref(), "agent");

        // Defaults: cwd falls back to home, per-request children, request-level
        // turn bound; base_url can still be given explicitly.
        let defaults = RuntimeConfig::parse(&acp_config("")).unwrap();
        let spec = defaults.acp_agents.get("agent").unwrap();
        assert!(spec.cwd.is_none());
        assert_eq!(spec.cwd(), Path::new("/tmp/agent-home"));
        assert_eq!(spec.mode, AcpSessionMode::PerRequest);
        assert_eq!(spec.turn_timeout, Duration::from_secs(120));
    }

    #[test]
    fn acp_agent_credentials_are_validated() {
        let swap = |from: &str, to: &str| RuntimeConfig::parse(&acp_config("").replace(from, to));
        for (from, to) in [
            ("provider = \"acp_agent\"", "provider = \"openai_compat\""),
            ("command = \"/usr/bin/env\"", "command = \"env\""),
            ("command = \"/usr/bin/env\"", "command = \"\""),
            ("home = \"/tmp/agent-home\"", "home = \"agent-home\""),
            ("home = \"/tmp/agent-home\"", "home = \"\""),
            ("AGENT_FLAG = \"on\"", "HOME = \"/elsewhere\""),
            ("AGENT_FLAG = \"on\"", "XDG_DATA_HOME = \"/elsewhere\""),
        ] {
            let err = swap(from, to)
                .err()
                .unwrap_or_else(|| panic!("accepted {to}"));
            assert!(err.to_string().contains("acp_agent"), "{to}: {err}");
        }
        for bad in [
            "cwd = \"work\"",
            "turn_timeout_ms = 0",
            "session_mode = \"forever\"",
        ] {
            assert!(
                RuntimeConfig::parse(&acp_config(bad)).is_err(),
                "accepted {bad}"
            );
        }
        // Env values may also come from file: references; bad names are rejected.
        let path = std::env::temp_dir().join(format!(
            "llmux-acp-env-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "from-file\n").unwrap();
        let loaded = RuntimeConfig::parse(&acp_config("").replace(
            "AGENT_FLAG = \"on\"",
            &format!("AGENT_FLAG = \"file:{}\"", path.display()),
        ))
        .unwrap();
        assert_eq!(loaded.acp_agents["agent"].env["AGENT_FLAG"], "from-file");
        std::fs::remove_file(path).unwrap();
        assert!(RuntimeConfig::parse(
            &acp_config("").replace("AGENT_FLAG = \"on\"", "\"BAD=NAME\" = \"x\"")
        )
        .is_err());
        // Credential values never leak into error messages.
        let err = RuntimeConfig::parse(&acp_config("").replace(
            "AGENT_KEY = \"env:LLMUX_CONFIG_OAUTH_TEST_SECRET\"",
            "AGENT_KEY = \"env:LLMUX_CONFIG_MISSING_ACP_SECRET\"",
        ))
        .map(|_| ())
        .unwrap_err();
        assert!(!err.to_string().contains("LLMUX_CONFIG_MISSING_ACP_SECRET"));
    }
}
