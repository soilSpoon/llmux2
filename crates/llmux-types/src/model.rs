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

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaSpec {
    pub rpm: Option<u32>,
    pub rpd: Option<u32>,
    pub tpm: Option<u32>,
    pub tpd: Option<u32>,
    pub concurrent: Option<u32>,
    /// Fixed anchored windows, for provider allowances that reset at a
    /// wall-clock boundary rather than sliding: weekly antigravity windows,
    /// UTC-midnight daily allowances, calendar-style monthly caps. Unlike
    /// the sliding `rpd`/`tpd` windows the whole budget returns at the
    /// boundary, so a fresh window can be front-loaded — matching what the
    /// provider actually grants.
    pub fixed: Vec<FixedWindowSpec>,
}

/// What a fixed window meters.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FixedUnit {
    Requests,
    Tokens,
}

/// One wall-clock-anchored fixed quota window: at most `max` units per
/// `span_secs`, counting only usage since the most recent boundary.
/// Boundaries are `anchor + k * span_secs` for every integer k, so the
/// anchor pins the phase (a weekly window starting Monday 00:00 UTC, a
/// daily allowance resetting at UTC midnight) rather than the first use.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FixedWindowSpec {
    pub unit: FixedUnit,
    pub max: u32,
    pub span_secs: u32,
    /// RFC 3339 timestamp of one window boundary; write it as a quoted
    /// string (`anchor = "2026-11-17T00:00:00Z"`).
    #[serde(deserialize_with = "parse_rfc3339", serialize_with = "format_rfc3339")]
    pub anchor: SystemTime,
}

fn parse_rfc3339<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    let parsed = time::OffsetDateTime::parse(&text, &time::format_description::well_known::Rfc3339)
        .map_err(serde::de::Error::custom)?;
    let nanos = parsed.unix_timestamp_nanos();
    let magnitude = u64::try_from(nanos.unsigned_abs())
        .map_err(|_| serde::de::Error::custom("timestamp out of range"))?;
    Ok(if nanos >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_nanos(magnitude)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_nanos(magnitude)
    })
}

fn format_rfc3339<S>(stamp: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let nanos: i128 = match stamp.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(err) => -(err.duration().as_nanos() as i128),
    };
    let text = time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(serde::ser::Error::custom)?
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(serde::ser::Error::custom)?;
    serializer.serialize_str(&text)
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
    Models,
    Plugins,
    Provider,
    ServiceTier,
    WebSearchOptions,
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
    /// Named `[[shared_quotas]]` family this leg draws on: legs that name the
    /// same group share one set of windows within the group's scope — the
    /// credential, or provider plus base URL for anonymous legs.
    pub shared_quota: Option<Arc<str>>,
    pub timeout: Duration,
    /// Context window in tokens (prompt plus completion); `None` is unchecked.
    pub max_context: Option<u32>,
    /// When this deployment stops being eligible, for time-limited offers.
    /// `None` means it never expires.
    pub expires_at: Option<SystemTime>,
    /// Effective priority while `priority_boost_until` is in the future;
    /// promotes legs carrying quota that dies at a deadline. `None` leaves
    /// ordering to `priority` alone.
    pub priority_boost: Option<i32>,
    /// Deadline after which `priority_boost` stops applying. Unlike
    /// `expires_at` the leg stays eligible — it only loses its promotion.
    pub priority_boost_until: Option<SystemTime>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_window_anchor_roundtrips_as_rfc3339() {
        let json =
            r#"{"unit":"requests","max":200,"span_secs":604800,"anchor":"2026-11-17T00:00:00Z"}"#;
        let window: FixedWindowSpec = serde_json::from_str(json).unwrap();
        assert_eq!(window.unit, FixedUnit::Requests);
        assert_eq!(window.max, 200);
        assert_eq!(window.span_secs, 604800);
        assert_eq!(
            window
                .anchor
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            1_794_873_600
        );
        let out = serde_json::to_string(&window).unwrap();
        let reparsed: FixedWindowSpec = serde_json::from_str(&out).unwrap();
        assert_eq!(reparsed, window);
        assert!(out.contains("2026-11-17T00:00:00Z"), "{out}");

        assert!(serde_json::from_str::<FixedWindowSpec>(
            r#"{"unit":"requests","max":200,"span_secs":604800,"anchor":"not-a-date"}"#
        )
        .is_err());
    }

    #[test]
    fn quota_spec_defaults_to_no_fixed_windows() {
        let spec: QuotaSpec = serde_json::from_str("{}").unwrap();
        assert!(spec.fixed.is_empty());
        assert!(serde_json::from_str::<QuotaSpec>(r#"{"fixed":[{"unit":"requests"}]}"#).is_err());
    }
}
