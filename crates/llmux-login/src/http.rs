use crate::{Error, Result};
use bytes::Bytes;
use http::{header, Request, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use llmux_types::{host_is_loopback, HttpClient};

use serde::de::DeserializeOwned;
use std::time::Duration;
use url::Url;

pub(crate) struct Http(HttpClient);

impl Http {
    pub(crate) fn new() -> Self {
        Self(llmux_types::http_client())
    }

    pub(crate) async fn json<T: DeserializeOwned>(&self, url: &Url) -> Result<T> {
        let (status, bytes) = self.response(url, None).await?;
        if status != StatusCode::OK {
            return Err(Error::Http);
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::Http)
    }

    pub(crate) async fn exchange<T: DeserializeOwned>(
        &self,
        url: &Url,
        form: String,
        client: &str,
    ) -> Result<T> {
        let (status, bytes) = self.response(url, Some(form)).await?;
        if status != StatusCode::OK {
            let error = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
            if status == StatusCode::BAD_REQUEST
                && error
                    .as_ref()
                    .is_some_and(|value| value["error"] == "invalid_grant")
            {
                return Err(Error::InvalidGrant {
                    client_id: client.into(),
                });
            }
            return Err(Error::Http);
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::Http)
    }

    async fn response(&self, url: &Url, form: Option<String>) -> Result<(StatusCode, Bytes)> {
        validate_url(url)?;
        let mut request = Request::builder()
            .uri(url.as_str())
            .header(header::ACCEPT, "application/json");
        let body = if let Some(form) = form {
            request = request
                .method("POST")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            Bytes::from(form)
        } else {
            Bytes::new()
        };
        let request = request.body(Full::new(body)).map_err(|_| Error::Http)?;
        tokio::time::timeout(Duration::from_secs(30), async {
            let response = self.0.request(request).await.map_err(|_| Error::Http)?;
            let status = response.status();
            let bytes = Limited::new(response.into_body(), 1024 * 1024)
                .collect()
                .await
                .map_err(|_| Error::Http)?
                .to_bytes();
            Ok((status, bytes))
        })
        .await
        .map_err(|_| Error::Http)?
    }
}

pub(crate) fn validate_url(url: &Url) -> Result<()> {
    let loopback = url.host_str().is_some_and(host_is_loopback);
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
    {
        return Err(Error::Endpoint);
    }
    Ok(())
}
