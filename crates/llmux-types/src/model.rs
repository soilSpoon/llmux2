use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeploymentIdx(pub u32);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProviderKind(pub Arc<str>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(pub Arc<str>);

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaSpec {
    pub rpm: Option<u32>,
    pub rpd: Option<u32>,
    pub tpm: Option<u32>,
    pub tpd: Option<u32>,
    pub concurrent: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    pub stream: bool,
    pub max_context: Option<u32>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            stream: true,
            max_context: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChatRequestField {
    Seed,
    User,
    SafetyIdentifier,
    #[serde(rename = "reasoning.effort")]
    ReasoningEffort,
    Logprobs,
    TopLogprobs,
    Temperature,
    TopP,
    TopK,
}

#[derive(Debug)]
pub struct Deployment {
    pub headers: http::HeaderMap,
    pub fallback: bool,
    pub idx: DeploymentIdx,
    pub id: Arc<str>,
    pub provider: ProviderKind,
    /// Empty for explicitly anonymous deployments.
    pub credential: Arc<str>,
    pub base_url: http::Uri,
    pub upstream_model: Arc<str>,
    pub strip_request_fields: Box<[ChatRequestField]>,
    pub priority: i32,
    pub quota: QuotaSpec,
    pub timeout: Duration,
    /// Context window in tokens (prompt plus completion); `None` is unchecked.
    pub max_context: Option<u32>,
    /// When this deployment stops being eligible, for time-limited offers.
    /// `None` means it never expires.
    pub expires_at: Option<SystemTime>,
    /// Whether probing this leg can spend money or quota; `Billed` is the
    /// safe default when the config does not say.
    pub billing: Billing,
}

/// Whether a leg of a model can be called at no cost. `Billed` covers paid
/// providers, billed credentials, and quota-capped legs (Workers AI's daily
/// cap), and is the default for configs that carry no marker.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    #[default]
    Billed,
    Free,
}

/// What to do with a request that fits no deployment's context window.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub enum ContextOverflow {
    #[default]
    #[serde(rename = "reject")]
    Reject,
    /// Drop messages from the middle, keeping system prompts and the latest message.
    #[serde(rename = "middle-out")]
    MiddleOut,
}

impl ContextOverflow {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "reject" => Some(Self::Reject),
            "middle-out" => Some(Self::MiddleOut),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct CanonicalModel {
    pub name: Arc<str>,
    pub capabilities: Capabilities,
    pub context_overflow: ContextOverflow,
    /// Sorted by ascending priority.
    pub deployments: Box<[DeploymentIdx]>,
}
