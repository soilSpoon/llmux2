//! Nous Portal device-code sign-in (RFC 8628), the flow Hermes Agent uses.
//!
//! Headless: it prints the verification URL and user code and polls. It never
//! opens a browser and never prints a token or the device code.

use crate::{http::Http, Error, Result};
use llmux_types::{NousState, NOUS_CLIENT_ID, NOUS_PORTAL_URL, NOUS_SCOPE};
use serde::Deserialize;
use std::{
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    expires_in: u64,
    #[serde(default)]
    interval: u64,
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    expires_in: u64,
}

pub struct NousLogin {
    pub portal: Url,
    pub client_id: String,
    pub scope: String,
    /// Floor for the poll interval; tests lower it.
    pub min_poll: Duration,
}

impl Default for NousLogin {
    fn default() -> Self {
        Self {
            portal: Url::parse(NOUS_PORTAL_URL).unwrap(),
            client_id: NOUS_CLIENT_ID.into(),
            scope: NOUS_SCOPE.into(),
            min_poll: Duration::from_secs(1),
        }
    }
}

impl NousLogin {
    fn endpoint(&self, path: &str) -> Result<Url> {
        Url::parse(&format!(
            "{}{path}",
            self.portal.as_str().trim_end_matches('/')
        ))
        .map_err(|_| Error::Endpoint)
    }

    async fn form(
        &self,
        http: &Http,
        path: &str,
        pairs: &[(&str, &str)],
    ) -> Result<(http::StatusCode, bytes::Bytes)> {
        let form = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        http.response(&self.endpoint(path)?, Some(form)).await
    }

    /// Requests a device code, calls `announce(verification_url, user_code)`,
    /// polls until approval, and atomically writes `state_path` (0600).
    pub async fn run(
        &self,
        state_path: &Path,
        announce: impl FnOnce(&str, &str),
    ) -> Result<NousState> {
        let http = Http::new();
        let (status, body) = self
            .form(
                &http,
                "/api/oauth/device/code",
                &[("client_id", &self.client_id), ("scope", &self.scope)],
            )
            .await?;
        if status != http::StatusCode::OK {
            return Err(Error::Http);
        }
        let device: DeviceCode = serde_json::from_slice(&body).map_err(|_| Error::Http)?;
        if device.device_code.is_empty() || device.verification_uri_complete.is_empty() {
            return Err(Error::Http);
        }
        announce(&device.verification_uri_complete, &device.user_code);
        let deadline = Instant::now() + Duration::from_secs(device.expires_in.max(1));
        let mut interval = Duration::from_secs(device.interval).max(self.min_poll);
        let tokens: Tokens = loop {
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            tokio::time::sleep(interval.min(deadline - Instant::now())).await;
            let (status, body) = self
                .form(
                    &http,
                    "/api/oauth/token",
                    &[
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ("client_id", &self.client_id),
                        ("device_code", &device.device_code),
                    ],
                )
                .await?;
            if status == http::StatusCode::OK {
                break serde_json::from_slice(&body).map_err(|_| Error::Http)?;
            }
            let error = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_owned));
            match error.as_deref() {
                Some("authorization_pending") => {}
                Some("slow_down") => {
                    interval = (interval + Duration::from_secs(1)).min(Duration::from_secs(30))
                }
                Some("access_denied") => return Err(Error::AccessDenied),
                Some("expired_token") => return Err(Error::Timeout),
                _ => return Err(Error::Authorization),
            }
        };
        if tokens.access_token.is_empty() || tokens.refresh_token.is_empty() {
            return Err(Error::Http);
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let state = NousState {
            portal_base_url: self.portal.as_str().trim_end_matches('/').to_owned(),
            client_id: self.client_id.clone(),
            scope: if tokens.scope.is_empty() {
                self.scope.clone()
            } else {
                tokens.scope
            },
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: now + tokens.expires_in,
        };
        let (path, saved) = (state_path.to_owned(), state.clone());
        tokio::task::spawn_blocking(move || {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
                }
            }
            saved.save(&path)
        })
        .await
        .map_err(|_| Error::Storage)?
        .map_err(|_| Error::Storage)?;
        Ok(state)
    }
}

pub fn snippet(state_path: &Path) -> String {
    let quote = |value: &str| toml::Value::String(value.to_owned()).to_string();
    format!(
        "[[credentials]]\nid = \"nous\"\nkind = \"nous_portal\"\nprovider = \"openai_compat\"\nstate_file = {}\n",
        quote(&state_path.display().to_string())
    )
}
