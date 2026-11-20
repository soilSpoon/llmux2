use serde::Deserialize;
use std::{sync::Arc, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeploymentIdx(pub u32);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProviderKind(pub Arc<str>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(pub Arc<str>);

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    Free,
    Subscription,
    #[default]
    PayAsYouGo,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaSpec {
    pub rpm: Option<u32>,
    pub rpd: Option<u32>,
    pub tpm: Option<u32>,
    pub tpd: Option<u32>,
    pub concurrent: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    #[default]
    Priority,
    Weighted,
    LeastUsed,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    pub stream: bool,
    pub tools: bool,
    pub vision: bool,
    pub max_context: Option<u32>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            stream: true,
            tools: false,
            vision: false,
            max_context: None,
        }
    }
}

#[derive(Debug)]
pub struct Deployment {
    pub idx: DeploymentIdx,
    pub id: Arc<str>,
    pub provider: ProviderKind,
    pub credential: Option<Arc<str>>,
    pub base_url: http::Uri,
    pub upstream_model: Arc<str>,
    pub priority: i32,
    pub weight: u32,
    pub billing: Billing,
    pub quota: QuotaSpec,
    pub timeout: Duration,
    /// Context window in tokens (prompt plus completion); `None` is unchecked.
    pub max_context: Option<u32>,
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
    pub strategy: Strategy,
    pub capabilities: Capabilities,
    pub context_overflow: ContextOverflow,
    /// Sorted by ascending priority.
    pub deployments: Box<[DeploymentIdx]>,
}
