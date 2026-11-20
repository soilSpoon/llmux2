//! Threshold alerting over the running gateway's admin stats.
//!
//! `llmux2-alert` reads `alerts.toml`, fetches `/admin/deployments`, evaluates
//! thresholds, persists a counter snapshot for delta-based checks, and reports
//! every breach as an `ALERT` line. Instantaneous checks (health, cooldown and
//! disabled counts) need no state; the failure ratio compares against the
//! previous snapshot, so the first run after a deploy or counter reset only
//! records a baseline.
use crate::{atomic_write, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Thresholds {
    /// Gateway listen address ("host:port"). Defaults to `active.json`'s.
    pub listen: Option<String>,
    /// File containing the dashboard bearer token for `/admin` requests.
    pub key_file: Option<PathBuf>,
    /// Alert when the share of failed attempts (failed_attempts +
    /// stream_errors) across new attempts since the last run exceeds this.
    pub failure_ratio: Option<f64>,
    /// Minimum new attempts before `failure_ratio` applies (default 10).
    pub min_attempts: u64,
    /// Alert when more deployments are in cooldown than this count.
    pub max_cooldowns: Option<u32>,
    /// Alert when more deployments are disabled than this count.
    pub max_disabled: Option<u32>,
}

impl Thresholds {
    pub fn load(path: &Path) -> std::result::Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let thresholds: Self = toml::from_str(&text).map_err(|e| e.to_string())?;
                if let Some(ratio) = thresholds.failure_ratio {
                    if !(0.0..=1.0).contains(&ratio) {
                        return Err(format!(
                            "failure_ratio must be between 0 and 1, got {ratio}"
                        ));
                    }
                }
                Ok(thresholds)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.to_string()),
        }
    }
    fn min_attempts(&self) -> u64 {
        if self.min_attempts == 0 {
            10
        } else {
            self.min_attempts
        }
    }
}

/// Host operations the alert check needs; `Host` implements it against the
/// live gateway, tests against a fake.
pub trait Monitor {
    /// GET `path` on `listen` ("host:port"); returns (status, body).
    fn get(&mut self, listen: &str, path: &str, bearer: Option<&str>) -> Result<(u16, Vec<u8>)>;
    fn health(&mut self, listen: &str) -> Result<bool>;
}

fn counter(entry: &Value, name: &str) -> u64 {
    entry[name].as_u64().unwrap_or(0)
}

fn snapshot(deployments: &[Value]) -> Value {
    let entries: Map<String, Value> = deployments
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(|id| (id.to_string(), entry)))
        .map(|(id, entry)| {
            (
                id,
                json!({
                    "routed": counter(entry, "routed"),
                    "failed_attempts": counter(entry, "failed_attempts"),
                    "stream_errors": counter(entry, "stream_errors"),
                }),
            )
        })
        .collect();
    json!({ "deployments": entries })
}

/// Evaluate thresholds against current deployment stats. `previous` is the
/// last snapshot's `deployments` object, if any.
pub fn evaluate(
    deployments: &[Value],
    previous: Option<&Map<String, Value>>,
    thresholds: &Thresholds,
) -> Vec<String> {
    let mut alerts = Vec::new();
    if let Some(max) = thresholds.max_disabled {
        let disabled: Vec<&str> = deployments
            .iter()
            .filter(|entry| entry["disabled"] == true)
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        if disabled.len() as u32 > max {
            alerts.push(format!(
                "disabled deployments {} > {max}: {}",
                disabled.len(),
                disabled.join(", ")
            ));
        }
    }
    if let Some(max) = thresholds.max_cooldowns {
        let cooling: Vec<&str> = deployments
            .iter()
            .filter(|entry| entry["cooldown_ms"].is_number())
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        if cooling.len() as u32 > max {
            alerts.push(format!(
                "cooldown deployments {} > {max}: {}",
                cooling.len(),
                cooling.join(", ")
            ));
        }
    }
    if let Some(max) = thresholds.failure_ratio {
        for entry in deployments {
            let Some(id) = entry["id"].as_str() else {
                continue;
            };
            let Some(before) = previous.and_then(|map| map.get(id)) else {
                continue;
            };
            let routed = counter(entry, "routed");
            let failed = counter(entry, "failed_attempts");
            let stream = counter(entry, "stream_errors");
            let base_routed = counter(before, "routed");
            let base_failed = counter(before, "failed_attempts");
            let base_stream = counter(before, "stream_errors");
            // A reload or restart resets counters; any single counter dropping
            // means the snapshot re-bases rather than mixing generations.
            if routed < base_routed || failed < base_failed || stream < base_stream {
                continue;
            }
            // `routed` already counts requests that later errored mid-stream,
            // so only failed_attempts adds attempts on top of it.
            let new_failed = (failed - base_failed) + (stream - base_stream);
            let attempts = (routed - base_routed) + (failed - base_failed);
            if attempts < thresholds.min_attempts() {
                continue;
            }
            let ratio = new_failed as f64 / attempts as f64;
            if ratio > max {
                alerts.push(format!(
                    "failure ratio {ratio:.3} > {max} on {id} ({new_failed}/{attempts} new attempts)"
                ));
            }
        }
    }
    alerts
}

/// Run one alert pass; returns the breaches found.
pub fn check<M: Monitor>(
    root: &Path,
    thresholds: &Thresholds,
    record: Option<&Value>,
    monitor: &mut M,
) -> Result<Vec<String>> {
    let listen = match thresholds.listen.as_deref() {
        Some(listen) => listen.to_string(),
        None => record
            .and_then(|record| record["listen"].as_str())
            .ok_or("no gateway listen address: set listen in alerts.toml or deploy first")?
            .to_string(),
    };
    let mut alerts = Vec::new();
    if !monitor.health(&listen)? {
        alerts.push(format!("healthz at {listen} did not return ok"));
    }
    let key = match &thresholds.key_file {
        Some(path) => Some(
            fs::read_to_string(path)
                .map_err(|e| format!("reading key_file {}: {e}", path.display()))?
                .trim()
                .to_string(),
        ),
        None => None,
    };
    match monitor.get(&listen, "/admin/deployments", key.as_deref()) {
        Ok((200, body)) => {
            let body: Value = serde_json::from_slice(&body)
                .map_err(|e| format!("invalid /admin/deployments response: {e}"))?;
            let deployments: Vec<Value> =
                body["deployments"].as_array().cloned().unwrap_or_default();
            let state_path = root.join("alert-state.json");
            let previous = fs::read(&state_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
            alerts.extend(evaluate(
                &deployments,
                previous
                    .as_ref()
                    .and_then(|state| state["deployments"].as_object()),
                thresholds,
            ));
            atomic_write(
                &state_path,
                |output| {
                    serde_json::to_writer(&mut *output, &snapshot(&deployments))
                        .map_err(io::Error::other)
                },
                0o600,
            )?;
        }
        Ok((status, _)) => alerts.push(format!(
            "admin deployments check at {listen} returned HTTP {status}"
        )),
        Err(error) => alerts.push(format!(
            "admin deployments check at {listen} failed: {error}"
        )),
    }
    Ok(alerts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dep(
        id: &str,
        routed: u64,
        failed: u64,
        stream: u64,
        disabled: bool,
        cooling: bool,
    ) -> Value {
        json!({
            "id": id, "routed": routed, "failed_attempts": failed,
            "stream_errors": stream, "disabled": disabled,
            "cooldown_ms": if cooling { json!(500) } else { Value::Null },
        })
    }
    fn prev(entries: &[(&str, u64, u64, u64)]) -> Map<String, Value> {
        entries
            .iter()
            .map(|(id, r, f, s)| {
                (
                    id.to_string(),
                    json!({"routed": r, "failed_attempts": f, "stream_errors": s}),
                )
            })
            .collect()
    }

    #[test]
    fn defaults_are_off_and_min_attempts_defaults_to_ten() {
        let thresholds = Thresholds::default();
        assert!(evaluate(&[dep("a", 100, 50, 50, true, true)], None, &thresholds).is_empty());
        assert_eq!(thresholds.min_attempts(), 10);
        assert!(toml::from_str::<Thresholds>("unknown = 1").is_err());
    }

    #[test]
    fn failure_ratio_uses_delta_and_window() {
        let mut thresholds = Thresholds {
            failure_ratio: Some(0.5),
            min_attempts: 5,
            ..Default::default()
        };
        let now = [
            dep("a", 110, 8, 0, false, false),
            dep("b", 50, 0, 0, false, false),
        ];
        let before = prev(&[("a", 100, 0, 0), ("b", 50, 0, 0)]);
        // a: 8/18 = 44% < 50%; b: no attempts.
        assert!(evaluate(&now, Some(&before), &thresholds).is_empty());
        let before = prev(&[("a", 100, 0, 0), ("gone", 1, 1, 0)]);
        let now = [
            dep("a", 108, 6, 0, false, false),
            dep("new", 0, 0, 0, false, false),
        ];
        // a: 6/14 = 43%; still below.
        assert!(evaluate(&now, Some(&before), &thresholds).is_empty());
        let now = [dep("a", 105, 6, 2, false, false)];
        // a: 8/11 = 72% > 50%; stream errors already count in routed.
        let alerts = evaluate(&now, Some(&before), &thresholds);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].contains("a") && alerts[0].contains("8/11"));
        // Ten routed streams all failing is 100%, not 50%.
        let all_streams_failed = [dep("a", 110, 0, 10, false, false)];
        let alerts = evaluate(&all_streams_failed, Some(&before), &thresholds);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].contains("10/10"));
        // Below the attempt window nothing fires.
        thresholds.min_attempts = 20;
        assert!(evaluate(&now, Some(&before), &thresholds).is_empty());
        // A counter reset skips the ratio and re-bases.
        let reset = [dep("a", 4, 1, 0, false, false)];
        assert!(evaluate(&reset, Some(&before), &thresholds).is_empty());
        // A drop in one failure counter is a reset even when the sum grows.
        let mixed = [dep("a", 105, 20, 0, false, false)];
        let before_mixed = prev(&[("a", 100, 5, 10)]);
        assert!(evaluate(&mixed, Some(&before_mixed), &thresholds).is_empty());
        // No snapshot: no ratio at all.
        assert!(evaluate(&now, None, &thresholds).is_empty());
    }

    #[test]
    fn load_rejects_invalid_failure_ratio() {
        let dir = std::env::temp_dir().join(format!("alert-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("alerts.toml");
        for text in [
            "failure_ratio = nan",
            "failure_ratio = -0.1",
            "failure_ratio = 1.5",
        ] {
            fs::write(&path, text).unwrap();
            assert!(Thresholds::load(&path).is_err(), "{text}");
        }
        fs::write(&path, "failure_ratio = 0.5").unwrap();
        assert_eq!(Thresholds::load(&path).unwrap().failure_ratio, Some(0.5));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn counts_compare_to_thresholds() {
        let thresholds = Thresholds {
            max_disabled: Some(1),
            max_cooldowns: Some(0),
            ..Default::default()
        };
        let now = [
            dep("a", 0, 0, 0, true, false),
            dep("b", 0, 0, 0, true, false),
            dep("c", 0, 0, 0, false, true),
        ];
        let alerts = evaluate(&now, None, &thresholds);
        assert_eq!(alerts.len(), 2);
        assert!(alerts[0].contains("disabled") && alerts[0].contains("a, b"));
        assert!(alerts[1].contains("cooldown") && alerts[1].contains('c'));
    }

    #[test]
    fn snapshot_roundtrip() {
        let deployments = vec![
            dep("a", 7, 2, 1, false, false),
            dep("b", 0, 0, 0, false, false),
        ];
        let state = snapshot(&deployments);
        assert_eq!(state["deployments"]["a"]["routed"], 7);
        assert_eq!(state["deployments"]["a"]["stream_errors"], 1);
        assert_eq!(state["deployments"]["b"]["failed_attempts"], 0);
    }
}
