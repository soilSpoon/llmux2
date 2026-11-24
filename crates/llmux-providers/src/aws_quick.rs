//! AWS Quick (`aws-quick`) through the magpie gateway the desktop plugin
//! exposes on loopback. The gateway speaks OpenAI Chat Completions and picks
//! the Quick agent from `agent_mode`; the model id is only a label, so this
//! adapter always sends the mode, never the AWS model id. Reference:
//! httpdone/magpie-aws-quick `lib/translate.mjs` (`modeFor`, `effortFor`) and
//! `index.mjs` (`x-magpie-effort`).

use crate::{
    messages,
    openai_compat::{apply_auth, bad_gateway, classify_error, collect_error, endpoint_uri},
    responses, sse,
};
use async_trait::async_trait;
use bytes::Bytes;
use http::{header, HeaderName, HeaderValue, Request, Response, StatusCode, Uri};
use hyper::body::Incoming;
use llmux_types::{
    provider, AuthMaterial, BuildError, Deployment, GatewayRequest, GatewayResponse, InboundFormat,
    ProviderAdapter, ProviderKind, Support, UpstreamError,
};
use serde_json::{json, Value};
use std::time::SystemTime;

/// Where the plugin's gateway listens; deployments usually name it as their
/// base URL, and this is the fallback when they do not.
const DEFAULT_BASE: &str = provider::AWS_QUICK_BASE_URL;
const PATH: &str = "/v1/chat/completions";
const AGENT_MODE: &str = "agent_mode";
const EFFORT_HEADER: &str = "x-magpie-effort";

const FAST: &str = "fast";
const BALANCED: &str = "balanced";
const SMART: &str = "smart";
const ADVANCED: &str = "advanced";

/// The AWS model id Quick resolves each documented mode to. Only `fast` and
/// `smart` have an id confirmed from the client; the other two modes travel
/// unchanged and Quick picks their model server-side.
const FAST_MODEL: &str = "us.anthropic.claude-haiku-5-5";
const SMART_MODEL: &str = "us.anthropic.claude-opus-5-5";

pub struct AwsQuick(ProviderKind);

impl AwsQuick {
    pub fn new() -> Self {
        Self(ProviderKind(provider::AWS_QUICK.into()))
    }
}

impl Default for AwsQuick {
    fn default() -> Self {
        Self::new()
    }
}

/// The agent mode a requested id selects. A model id names its mode, a bare
/// mode passes through, and anything else is `smart` — the same default the
/// reference client applies.
fn agent_mode(model: &str) -> &'static str {
    let bare = model.rsplit('/').next().unwrap_or(model).trim();
    match bare.to_ascii_lowercase().as_str() {
        FAST => FAST,
        BALANCED => BALANCED,
        SMART => SMART,
        ADVANCED => ADVANCED,
        FAST_MODEL => FAST,
        SMART_MODEL => SMART,
        _ => SMART,
    }
}

/// The AWS model id quick serves for a mode, where one is documented.
pub fn model_for(mode: &str) -> Option<&'static str> {
    match mode {
        FAST => Some(FAST_MODEL),
        SMART => Some(SMART_MODEL),
        _ => None,
    }
}

/// A thinking effort in the gateway's own vocabulary, from a client spelling.
fn effort(value: &str) -> Option<&'static str> {
    Some(match value.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "disabled" | "false" => "off",
        "low" | "minimal" => "low",
        "medium" | "med" => "medium",
        "high" | "enabled" | "true" => "high",
        "max" | "xhigh" => "max",
        _ => return None,
    })
}

/// The outbound Chat Completions body: the requested mode as both the model
/// and `agent_mode`, with any client effort moved to Quick's spelling.
fn payload(req: &GatewayRequest, mode: &str) -> Result<(Bytes, Option<&'static str>), BuildError> {
    let translated = match req.format {
        InboundFormat::OpenAiChat => req.body.clone(),
        InboundFormat::AnthropicMessages => messages::request(&req.body, mode)?,
        InboundFormat::OpenAiResponses => responses::request(&req.body, mode)?,
    };
    let mut value: Value =
        serde_json::from_slice(&translated).map_err(|_| BuildError::InvalidBody)?;
    let object = value.as_object_mut().ok_or(BuildError::InvalidBody)?;
    if !object.get("messages").is_some_and(Value::is_array) {
        return Err(BuildError::InvalidBody);
    }
    let effort = ["reasoning_effort", "thinking_effort"]
        .iter()
        .find_map(|field| object.get(*field).and_then(Value::as_str).and_then(effort));
    object.remove("reasoning_effort");
    match effort {
        Some(effort) => {
            object.insert("thinking_effort".into(), json!(effort));
        }
        None => {
            object.remove("thinking_effort");
        }
    }
    object.insert("model".into(), json!(mode));
    object.insert(AGENT_MODE.into(), json!(mode));
    Ok((Bytes::from(value.to_string()), effort))
}

fn endpoint(dep: &Deployment) -> Result<Uri, BuildError> {
    if dep.base_url.host().is_some() {
        return endpoint_uri(&dep.base_url, PATH);
    }
    endpoint_uri(&DEFAULT_BASE.parse()?, PATH)
}

#[async_trait]
impl ProviderAdapter for AwsQuick {
    fn kind(&self) -> &ProviderKind {
        &self.0
    }

    fn supports(&self, inbound: InboundFormat) -> Support {
        match inbound {
            InboundFormat::OpenAiChat => Support::Passthrough,
            InboundFormat::OpenAiResponses | InboundFormat::AnthropicMessages => Support::Convert,
        }
    }

    async fn build_request(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        auth: &AuthMaterial,
    ) -> Result<Request<Bytes>, BuildError> {
        let mode = agent_mode(crate::upstream_model(req, dep));
        let (body, effort) = payload(req, mode)?;
        let mut request = Request::post(endpoint(dep)?)
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                header::ACCEPT,
                if req.stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .body(body)?;
        request.headers_mut().extend(dep.headers.clone());
        if !request.headers().contains_key(EFFORT_HEADER) {
            if let Some(effort) = effort {
                request.headers_mut().insert(
                    HeaderName::from_static(EFFORT_HEADER),
                    HeaderValue::from_static(effort),
                );
            }
        }
        llmux_types::default_user_agent(request.headers_mut());
        apply_auth(
            request.headers_mut(),
            auth,
            header::AUTHORIZATION,
            "Bearer ",
        )?;
        Ok(request)
    }

    async fn parse_response(
        &self,
        req: &GatewayRequest,
        _dep: &Deployment,
        resp: Response<Incoming>,
    ) -> Result<GatewayResponse, UpstreamError> {
        let (parts, body) = resp.into_parts();
        let status = parts.status;
        if !status.is_success() {
            let body = collect_error(body).await;
            return Err(classify_error(
                status,
                &parts.headers,
                body,
                SystemTime::now(),
            ));
        }
        if req.stream {
            if !sse::is_event_stream(&parts.headers) {
                return Err(bad_gateway());
            }
            let stream = sse::decode(body);
            return Ok(GatewayResponse::Stream(match req.format {
                InboundFormat::AnthropicMessages => messages::stream(stream),
                InboundFormat::OpenAiResponses => responses::stream(stream),
                InboundFormat::OpenAiChat => stream,
            }));
        }
        let body = crate::openai_compat::collect(body).await?;
        let body = match req.format {
            InboundFormat::AnthropicMessages => messages::complete(&body)?,
            InboundFormat::OpenAiResponses => {
                responses::complete(&body).map_err(|()| bad_gateway())?
            }
            InboundFormat::OpenAiChat => body,
        };
        Ok(GatewayResponse::Complete {
            status: StatusCode::OK,
            headers: Default::default(),
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use futures_util::StreamExt;
    use http::HeaderMap;
    use http_body_util::Full;
    use hyper_util::{client::legacy::Client, rt::TokioExecutor};
    use llmux_types::{Billing, DeploymentIdx, QuotaSpec, RequestId};
    use std::{sync::Arc, time::Duration};

    fn dep(mode: &str) -> Deployment {
        Deployment {
            headers: HeaderMap::new(),
            fallback: false,
            idx: DeploymentIdx(0),
            id: "quick".into(),
            provider: ProviderKind(provider::AWS_QUICK.into()),
            credential: "key".into(),
            base_url: DEFAULT_BASE.parse().unwrap(),
            upstream_model: mode.into(),
            strip_request_fields: Box::default(),
            shared_quota: None,
            priority: 0,
            quota: QuotaSpec::default(),
            timeout: Duration::from_secs(30),
            max_context: None,
            expires_at: None,
            priority_boost: None,
            priority_boost_until: None,
            billing: Billing::Billed,
        }
    }

    fn request(body: Value, stream: bool) -> GatewayRequest {
        inbound(body, stream, InboundFormat::OpenAiChat)
    }

    fn inbound(body: Value, stream: bool, format: InboundFormat) -> GatewayRequest {
        GatewayRequest {
            format,
            model: "quick".into(),
            stream,
            body: Bytes::from(body.to_string()),
            anthropic_headers: HeaderMap::new(),
            est_input_tokens: 1,
            max_output_tokens: None,
            context_overflow: None,
            request_id: RequestId("req".into()),
            end_user: None,
            client_key: None,
            image_count: 0,
            free_only: false,
        }
    }

    #[test]
    fn mode_selects_the_documented_quick_model() {
        assert_eq!(model_for("fast"), Some("us.anthropic.claude-haiku-5-5"));
        assert_eq!(model_for("smart"), Some("us.anthropic.claude-opus-5-5"));
        assert_eq!(model_for("balanced"), None);
        for (model, mode) in [
            ("fast", "fast"),
            ("aws-quick/fast", "fast"),
            ("us.anthropic.claude-haiku-5-5", "fast"),
            ("smart", "smart"),
            ("us.anthropic.claude-opus-5-5", "smart"),
            ("advanced", "advanced"),
            ("unknown-model", "smart"),
        ] {
            assert_eq!(agent_mode(model), mode, "{model}");
        }
    }

    #[test]
    fn effort_moves_to_the_gateway_vocabulary() {
        assert_eq!(effort("xhigh"), Some("max"));
        assert_eq!(effort("minimal"), Some("low"));
        assert_eq!(effort("none"), Some("off"));
        assert_eq!(effort("HIGH"), Some("high"));
        assert_eq!(effort("bogus"), None);
    }

    #[tokio::test]
    async fn request_sends_the_agent_mode_and_effort() {
        let req = request(
            json!({"model":"whatever","stream":true,"reasoning_effort":"xhigh",
                   "messages":[{"role":"user","content":"hi"}]}),
            true,
        );
        let built = AwsQuick::new()
            .build_request(&req, &dep("fast"), &AuthMaterial::None)
            .await
            .unwrap();
        assert_eq!(
            built.uri().to_string(),
            "http://127.0.0.1:3425/v1/chat/completions"
        );
        assert_eq!(built.headers()[EFFORT_HEADER], "max");
        let body: Value = serde_json::from_slice(built.body()).unwrap();
        assert_eq!(body["model"], "fast");
        assert_eq!(body[AGENT_MODE], "fast");
        assert_eq!(body["thinking_effort"], "max");
        assert_eq!(body["messages"][0]["content"], "hi");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[tokio::test]
    async fn configured_effort_header_wins_over_the_body() {
        let mut upgrade = dep("unknown-model");
        upgrade
            .headers
            .insert(EFFORT_HEADER, HeaderValue::from_static("low"));
        let req = request(
            json!({"model":"x","reasoning_effort":"high","messages":[{"role":"user","content":"hi"}]}),
            false,
        );
        let built = AwsQuick::new()
            .build_request(&req, &upgrade, &AuthMaterial::Bearer(Arc::from("tok")))
            .await
            .unwrap();
        assert_eq!(built.headers()[EFFORT_HEADER], "low");
        assert_eq!(built.headers()["authorization"], "Bearer tok");
        let body: Value = serde_json::from_slice(built.body()).unwrap();
        assert_eq!(body[AGENT_MODE], "smart");
        assert_eq!(body["thinking_effort"], "high");
    }

    async fn upstream() -> (llmux_testkit::TestServer, Deployment) {
        let router = Router::new().route(
            "/v1/chat/completions",
            post(|Json(body): Json<Value>| async move {
                let mode = body["agent_mode"].as_str().unwrap().to_owned();
                if body["stream"] == json!(true) {
                    let payload = format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({"model":mode,"choices":[{"delta":{"role":"assistant","content":"hi"}}]})
                    );
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        payload,
                    )
                } else {
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        json!({
                            "id":"chatcmpl-quick",
                            "object":"chat.completion",
                            "created":1,
                            "model":mode,
                            "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
                            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}
                        })
                            .to_string(),
                    )
                }
            }),
        );
        let server = llmux_testkit::TestServer::start(router).await.unwrap();
        let mut deployment = dep("aws-quick/fast");
        deployment.base_url = server.url().parse().unwrap();
        (server, deployment)
    }

    #[tokio::test]
    async fn answers_pass_through_as_chat_completions() {
        let (_server, deployment) = upstream().await;
        let client = Client::builder(TokioExecutor::new()).build_http();
        let adapter = AwsQuick::new();
        let req = request(
            json!({"model":"quick","messages":[{"role":"user","content":"hi"}]}),
            false,
        );
        let built = adapter
            .build_request(&req, &deployment, &AuthMaterial::None)
            .await
            .unwrap();
        let resp = client.request(built.map(Full::new)).await.unwrap();
        match adapter
            .parse_response(&req, &deployment, resp)
            .await
            .unwrap()
        {
            GatewayResponse::Complete { body, .. } => {
                let value: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(value["model"], "fast");
                assert_eq!(value["choices"][0]["message"]["content"], "hi");
            }
            GatewayResponse::Stream(_) => panic!("expected a complete body"),
        }
    }

    #[tokio::test]
    async fn responses_requests_translate_effort_and_return_responses() {
        let (_server, deployment) = upstream().await;
        let req = inbound(
            json!({"model":"quick","input":"hello","reasoning":{"effort":"xhigh"}}),
            false,
            InboundFormat::OpenAiResponses,
        );
        let adapter = AwsQuick::new();
        let built = adapter
            .build_request(&req, &deployment, &AuthMaterial::None)
            .await
            .unwrap();
        assert_eq!(built.headers()[EFFORT_HEADER], "max");
        let outgoing: Value = serde_json::from_slice(built.body()).unwrap();
        assert_eq!(outgoing[AGENT_MODE], "fast");
        assert_eq!(outgoing["thinking_effort"], "max");
        assert_eq!(
            outgoing["messages"],
            json!([{"role":"user","content":"hello"}])
        );

        let client = Client::builder(TokioExecutor::new()).build_http();
        let resp = client.request(built.map(Full::new)).await.unwrap();
        let GatewayResponse::Complete { body, .. } = adapter
            .parse_response(&req, &deployment, resp)
            .await
            .unwrap()
        else {
            panic!("expected a complete Responses body");
        };
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["object"], "response");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(value["output"][0]["content"][0]["text"], "hi");
    }

    #[tokio::test]
    async fn anthropic_requests_return_messages() {
        let (_server, deployment) = upstream().await;
        let req = inbound(
            json!({
                "model":"quick","max_tokens":32,"system":"Be brief",
                "messages":[{"role":"user","content":"hello"}]
            }),
            false,
            InboundFormat::AnthropicMessages,
        );
        let adapter = AwsQuick::new();
        let built = adapter
            .build_request(&req, &deployment, &AuthMaterial::None)
            .await
            .unwrap();
        let outgoing: Value = serde_json::from_slice(built.body()).unwrap();
        assert_eq!(
            outgoing["messages"],
            json!([{"role":"system","content":"Be brief"},{"role":"user","content":"hello"}])
        );

        let client = Client::builder(TokioExecutor::new()).build_http();
        let resp = client.request(built.map(Full::new)).await.unwrap();
        let GatewayResponse::Complete { body, .. } = adapter
            .parse_response(&req, &deployment, resp)
            .await
            .unwrap()
        else {
            panic!("expected a complete Messages body");
        };
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["type"], "message");
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["stop_reason"], "end_turn");
        assert_eq!(value["content"], json!([{"type":"text","text":"hi"}]));
        assert_eq!(value["usage"], json!({"input_tokens":3,"output_tokens":2}));
    }

    #[tokio::test]
    async fn streams_stay_event_stream() {
        let (_server, deployment) = upstream().await;
        let client = Client::builder(TokioExecutor::new()).build_http();
        let adapter = AwsQuick::new();
        let req = request(
            json!({"model":"quick","stream":true,"messages":[{"role":"user","content":"hi"}]}),
            true,
        );
        let built = adapter
            .build_request(&req, &deployment, &AuthMaterial::None)
            .await
            .unwrap();
        let resp = client.request(built.map(Full::new)).await.unwrap();
        let GatewayResponse::Stream(mut events) = adapter
            .parse_response(&req, &deployment, resp)
            .await
            .unwrap()
        else {
            panic!("expected a stream");
        };
        let mut data = Vec::new();
        while let Some(event) = events.next().await {
            data.push(event.unwrap().data);
        }
        assert_eq!(data.len(), 2);
        let first: Value = serde_json::from_str(&data[0]).unwrap();
        assert_eq!(first["choices"][0]["delta"]["content"], "hi");
    }

    #[tokio::test]
    async fn rate_limits_map_to_the_gateway_error() {
        let router = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, "3")],
                    Json(json!({"error":{"message":"slow down","type":"rate_limit"}})),
                )
            }),
        );
        let server = llmux_testkit::TestServer::start(router).await.unwrap();
        let mut deployment = dep("smart");
        deployment.base_url = server.url().parse().unwrap();
        let client = Client::builder(TokioExecutor::new()).build_http();
        let adapter = AwsQuick::new();
        let req = request(
            json!({"model":"quick","messages":[{"role":"user","content":"hi"}]}),
            false,
        );
        let built = adapter
            .build_request(&req, &deployment, &AuthMaterial::None)
            .await
            .unwrap();
        let resp = client.request(built.map(Full::new)).await.unwrap();
        let error = match adapter.parse_response(&req, &deployment, resp).await {
            Err(error) => error,
            Ok(_) => panic!("expected an error"),
        };
        assert!(
            matches!(error, UpstreamError::RateLimited { retry_after: Some(wait) } if wait == Duration::from_secs(3)),
            "{error:?}"
        );
    }
}
