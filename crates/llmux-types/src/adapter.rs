use crate::{
    AuthError, BuildError, Deployment, GatewayRequest, GatewayResponse, InboundFormat,
    ProviderKind, UpstreamError,
};
use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response};
use hyper::body::Incoming;
use std::{collections::HashMap, sync::Arc, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    Passthrough,
    Convert,
    No,
}

#[derive(Clone)]
pub enum AuthMaterial {
    Bearer(Arc<str>),
    Header {
        name: HeaderName,
        value: HeaderValue,
    },
    None,
}

#[async_trait]
pub trait CredentialSource: Send + Sync {
    fn current(&self) -> Result<AuthMaterial, AuthError>;

    async fn refresh(&self) -> Result<(), AuthError>;
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync + 'static {
    fn kind(&self) -> &ProviderKind;

    fn supports(&self, inbound: InboundFormat) -> Support;

    /// Whether routing may continue to another deployment after this adapter
    /// reports exhausted quota. When this is `false`, the active request stops
    /// after that error and later billed candidates stay blocked while the
    /// quota cooldown is active.
    fn fallback_on_quota_exhausted(&self) -> bool {
        true
    }

    async fn build_request(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        auth: &AuthMaterial,
    ) -> Result<Request<Bytes>, BuildError>;

    async fn parse_response(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        resp: Response<Incoming>,
    ) -> Result<GatewayResponse, UpstreamError>;

    /// Serves the request without an HTTP round trip (for example a local
    /// subprocess). `None`, the default, means the executor sends the request
    /// from `build_request` upstream and calls `parse_response`.
    async fn invoke(
        &self,
        _req: &GatewayRequest,
        _dep: &Deployment,
    ) -> Option<Result<GatewayResponse, UpstreamError>> {
        None
    }

    /// Called when the upstream rejected a request body with `error`.
    /// Returning a corrected body makes the executor resend it to the same
    /// deployment, never to another one; `None`, the default, surfaces the
    /// error. Each call must remove something from the body so retries end.
    fn repair_rejected(&self, _body: &Bytes, _error: &UpstreamError) -> Option<Bytes> {
        None
    }

    fn observe_limits(&self, _headers: &HeaderMap) -> Option<LimitObservation> {
        None
    }

    /// Provider-side request id found in response `headers`, then in the
    /// response body's `id`/`request_id` when the body is the provider's
    /// own payload. Adapters that synthesize the body must override this
    /// to look at headers only.
    fn upstream_request_id(&self, headers: &HeaderMap, body: Option<&Bytes>) -> Option<Arc<str>> {
        header_request_id(headers).or_else(|| body.and_then(body_request_id))
    }
}

/// Request id carried in upstream response headers.
pub fn header_request_id(headers: &HeaderMap) -> Option<Arc<str>> {
    ["anthropic-request-id", "x-request-id", "request-id"]
        .iter()
        .filter_map(|name| headers.get(*name))
        .filter_map(|value| value.to_str().ok())
        .find(|value| !value.is_empty())
        .map(Arc::from)
}

/// Request id carried in a provider-owned JSON body: `request_id`, `id`,
/// or `error.request_id`.
pub fn body_request_id(body: &Bytes) -> Option<Arc<str>> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    for key in ["request_id", "id"] {
        if let Some(id) = value.get(key).and_then(|v| v.as_str()) {
            if !id.is_empty() {
                return Some(Arc::from(id));
            }
        }
    }
    value
        .get("error")
        .and_then(|error| error.get("request_id"))
        .and_then(|v| v.as_str())
        .filter(|id| !id.is_empty())
        .map(Arc::from)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimitObservation {
    pub remaining_requests: Option<u32>,
    pub remaining_tokens: Option<u32>,
    pub reset_requests: Option<Duration>,
    pub reset_tokens: Option<Duration>,
}

#[derive(Default)]
pub struct ProviderRegistry {
    adapters: HashMap<ProviderKind, Arc<dyn ProviderAdapter>>,
}

impl ProviderRegistry {
    pub fn register(
        &mut self,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> Option<Arc<dyn ProviderAdapter>> {
        self.adapters.insert(adapter.kind().clone(), adapter)
    }

    pub fn get(&self, kind: &ProviderKind) -> Option<&Arc<dyn ProviderAdapter>> {
        self.adapters.get(kind)
    }
}
