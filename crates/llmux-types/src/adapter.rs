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

    fn observe_limits(&self, _headers: &HeaderMap) -> Option<LimitObservation> {
        None
    }
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
