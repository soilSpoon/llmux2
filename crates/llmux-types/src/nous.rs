//! On-disk login state for the Nous Portal credential source.
//!
//! `llmux-login nous` writes it and the gateway's `nous_portal` credential
//! source reads and rotates it. Secrets intentionally do not implement Debug.

use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

pub const NOUS_PORTAL_URL: &str = "https://portal.nousresearch.com";
pub const NOUS_CLIENT_ID: &str = "hermes-cli";
pub const NOUS_SCOPE: &str = "inference:invoke";
pub const NOUS_INFERENCE_URL: &str = "https://inference-api.nousresearch.com/v1";

#[derive(Clone, Serialize, Deserialize)]
pub struct NousState {
    pub portal_base_url: String,
    pub client_id: String,
    pub scope: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds at which `access_token` stops working.
    pub expires_at: u64,
}

impl NousState {
    pub fn load(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        let state: Self = serde_json::from_slice(&bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid Nous state file"))?;
        if state.refresh_token.is_empty() || state.client_id.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Nous state file has no refresh token",
            ));
        }
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "could not encode state"))?;
        crate::atomic_write(path, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> NousState {
        NousState {
            portal_base_url: "http://127.0.0.1:1".into(),
            client_id: NOUS_CLIENT_ID.into(),
            scope: NOUS_SCOPE.into(),
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 7,
        }
    }

    #[test]
    fn save_is_private_and_round_trips_and_rejects_empty_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        state().save(&path).unwrap();
        assert_eq!(NousState::load(&path).unwrap().expires_at, 7);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        NousState {
            refresh_token: String::new(),
            ..state()
        }
        .save(&path)
        .unwrap();
        assert!(NousState::load(&path).is_err());
        assert!(NousState::load(&dir.path().join("missing")).is_err());
    }
}
