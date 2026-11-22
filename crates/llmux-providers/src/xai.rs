//! xAI OAuth adapter. Chat Completions and Responses share `https://api.x.ai/v1`;
//! the model id picks the endpoint. Client effort suffixes are not upstream ids.

use crate::{openai_compat::OpenAiCompat, OpenAiResponses};
use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, Request, Response};
use hyper::body::Incoming;
use llmux_types::{
    provider, AuthMaterial, BuildError, Deployment, GatewayRequest, GatewayResponse, InboundFormat,
    LimitObservation, ProviderAdapter, ProviderKind, Support, UpstreamError,
};
use serde_json::{json, Value};

const EFFORTS: &[&str] = &["xhigh", "high", "medium", "low"];
const RESPONSES: &[&str] = &["grok-4.5", "grok-4.6", "grok-4.20-multi-agent-0309"];
const EFFORT_ALLOWED: &[&str] = &["grok-4.3", "grok-4.20-0309-reasoning"];
const EFFORT_DENIED: &[&str] = &["grok-build-0.1", "grok-4.20-0309-non-reasoning"];

pub struct Xai(ProviderKind);

impl Xai {
    pub fn new() -> Self {
        Self(ProviderKind(provider::XAI.into()))
    }
}

impl Default for Xai {
    fn default() -> Self {
        Self::new()
    }
}

fn stripped(model: &str) -> (String, Option<&str>) {
    for effort in EFFORTS {
        let suffix = format!("-{effort}");
        if let Some(base) = model.strip_suffix(&suffix) {
            let effort = EFFORT_ALLOWED.contains(&base).then_some(*effort);
            return (base.to_owned(), effort);
        }
    }
    (model.to_owned(), None)
}

fn responses(model: &str) -> bool {
    RESPONSES.contains(&model)
}

fn with_model(req: &GatewayRequest, body: Bytes, model: &str) -> GatewayRequest {
    GatewayRequest {
        format: req.format,
        model: model.into(),
        stream: req.stream,
        body,
        anthropic_headers: req.anthropic_headers.clone(),
        client_key: req.client_key.clone(),
        image_count: req.image_count,
        est_input_tokens: req.est_input_tokens,
        max_output_tokens: req.max_output_tokens,
        context_overflow: req.context_overflow,
        request_id: req.request_id.clone(),
        end_user: req.end_user.clone(),
    }
}

fn chat_body(body: &[u8], model: &str, effort: Option<&str>) -> Result<Bytes, BuildError> {
    let mut value: Value = serde_json::from_slice(body).map_err(|_| BuildError::InvalidBody)?;
    let object = value.as_object_mut().ok_or(BuildError::InvalidBody)?;
    let denied = EFFORT_DENIED.contains(&model);
    if denied {
        object.remove("reasoning_effort");
    } else if let Some(effort) = effort {
        object.insert("reasoning_effort".into(), json!(effort));
    }
    object.insert("model".into(), json!(model));
    Ok(Bytes::from(value.to_string()))
}

#[async_trait]
impl ProviderAdapter for Xai {
    fn kind(&self) -> &ProviderKind {
        &self.0
    }

    fn supports(&self, inbound: InboundFormat) -> Support {
        OpenAiCompat::new(provider::XAI).supports(inbound)
    }

    async fn build_request(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        auth: &AuthMaterial,
    ) -> Result<Request<Bytes>, BuildError> {
        let (model, effort) = stripped(crate::upstream_model(req, dep));
        let dep = Deployment {
            upstream_model: model.clone().into(),
            headers: dep.headers.clone(),
            fallback: dep.fallback,
            idx: dep.idx,
            id: dep.id.clone(),
            provider: dep.provider.clone(),
            credential: dep.credential.clone(),
            base_url: dep.base_url.clone(),
            strip_request_fields: dep.strip_request_fields.clone(),
            priority: dep.priority,
            quota: dep.quota,
            timeout: dep.timeout,
            max_context: dep.max_context,
            expires_at: dep.expires_at,
        };
        if responses(&model) {
            let mut request = OpenAiResponses::new()
                .build_request(req, &dep, auth)
                .await?;
            if let Some(effort) = effort {
                let mut body: Value =
                    serde_json::from_slice(request.body()).map_err(|_| BuildError::InvalidBody)?;
                body["reasoning"]["effort"] = json!(effort);
                *request.body_mut() = Bytes::from(body.to_string());
            }
            return Ok(request);
        }
        let body = if req.format == InboundFormat::OpenAiChat {
            chat_body(&req.body, &model, effort)?
        } else {
            req.body.clone()
        };
        let request = with_model(req, body, &model);
        OpenAiCompat::new(provider::XAI)
            .build_request(&request, &dep, auth)
            .await
    }

    async fn parse_response(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        resp: Response<Incoming>,
    ) -> Result<GatewayResponse, UpstreamError> {
        let (model, _) = stripped(crate::upstream_model(req, dep));
        if responses(&model) {
            OpenAiResponses::new().parse_response(req, dep, resp).await
        } else {
            OpenAiCompat::new(provider::XAI)
                .parse_response(req, dep, resp)
                .await
        }
    }

    fn observe_limits(&self, headers: &HeaderMap) -> Option<LimitObservation> {
        OpenAiCompat::new(provider::XAI).observe_limits(headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llmux_types::{DeploymentIdx, QuotaSpec, RequestId};
    use std::time::Duration;

    fn dep(model: &str) -> Deployment {
        Deployment {
            headers: HeaderMap::new(),
            fallback: false,
            idx: DeploymentIdx(0),
            id: "xai".into(),
            provider: ProviderKind(provider::XAI.into()),
            credential: "oauth".into(),
            base_url: "https://api.x.ai/v1".parse().unwrap(),
            upstream_model: model.into(),
            strip_request_fields: Box::default(),
            priority: 0,
            quota: QuotaSpec::default(),
            timeout: Duration::from_secs(30),
            max_context: None,
            expires_at: None,
        }
    }

    fn request(body: Value) -> GatewayRequest {
        GatewayRequest {
            format: InboundFormat::OpenAiChat,
            model: "public".into(),
            stream: false,
            body: Bytes::from(body.to_string()),
            anthropic_headers: HeaderMap::new(),
            est_input_tokens: 1,
            max_output_tokens: None,
            context_overflow: None,
            request_id: RequestId("req".into()),
            end_user: None,
            client_key: None,
            image_count: 0,
        }
    }

    #[tokio::test]
    async fn responses_models_keep_the_unsuffixed_id() {
        let adapter = Xai::new();
        let built = adapter
            .build_request(
                &request(json!({"messages":[{"role":"user","content":"hi"}]})),
                &dep("grok-4.6-high"),
                &AuthMaterial::Bearer("synthetic-token".into()),
            )
            .await
            .unwrap();
        assert!(built.uri().to_string().ends_with("/v1/responses"));
        let body: Value = serde_json::from_slice(built.body()).unwrap();
        assert_eq!(body["model"], "grok-4.6");
        assert_eq!(built.headers()["authorization"], "Bearer synthetic-token");
    }

    #[tokio::test]
    async fn chat_effort_suffix_maps_to_reasoning_effort() {
        let adapter = Xai::new();
        let built = adapter
            .build_request(
                &request(json!({"messages":[{"role":"user","content":"hi"}]})),
                &dep("grok-4.3-high"),
                &AuthMaterial::Bearer("synthetic-token".into()),
            )
            .await
            .unwrap();
        assert!(built.uri().to_string().ends_with("/chat/completions"));
        let body: Value = serde_json::from_slice(built.body()).unwrap();
        assert_eq!(body["model"], "grok-4.3");
        assert_eq!(body["reasoning_effort"], "high");
    }
}
