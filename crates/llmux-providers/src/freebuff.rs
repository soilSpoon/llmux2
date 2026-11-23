//! Freebuff uses the Codebuff SDK's OpenAI-compatible Chat Completions wire shape.
//!
//! The upstream SDK's `sdk/src/impl/model-provider.ts` joins `/api/v1` to
//! the hosted origin and uses a Codebuff bearer key. The production smoke
//! configuration in `freebuff/e2e/README.md` sets that origin to
//! `https://www.codebuff.com`. Free-mode admission is separate from the wire
//! protocol and remains the upstream's responsibility.

use crate::OpenAiCompat;
use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, Request, Response};
use hyper::body::Incoming;
use llmux_types::{
    provider, AuthMaterial, BuildError, Deployment, GatewayRequest, GatewayResponse, InboundFormat,
    LimitObservation, ProviderAdapter, ProviderKind, Support, UpstreamError,
};

pub struct Freebuff(OpenAiCompat);

impl Default for Freebuff {
    fn default() -> Self {
        Self(OpenAiCompat::new(provider::FREEBUFF))
    }
}

impl Freebuff {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ProviderAdapter for Freebuff {
    fn kind(&self) -> &ProviderKind {
        self.0.kind()
    }

    fn supports(&self, inbound: InboundFormat) -> Support {
        self.0.supports(inbound)
    }

    async fn build_request(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        auth: &AuthMaterial,
    ) -> Result<Request<Bytes>, BuildError> {
        self.0.build_request(req, dep, auth).await
    }

    async fn parse_response(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        resp: Response<Incoming>,
    ) -> Result<GatewayResponse, UpstreamError> {
        self.0.parse_response(req, dep, resp).await
    }

    fn observe_limits(&self, headers: &HeaderMap) -> Option<LimitObservation> {
        self.0.observe_limits(headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{response::IntoResponse, routing::post, Json, Router};
    use futures_util::StreamExt;
    use http::{header, StatusCode};
    use http_body_util::Full;
    use hyper_util::{client::legacy::Client, rt::TokioExecutor};
    use llmux_testkit::TestServer;
    use llmux_types::{Billing, DeploymentIdx, QuotaSpec, RequestId};
    use serde_json::{json, Value};
    use std::time::Duration;

    const MODEL: &str = "deepseek/deepseek-v4-flash";
    const CHUNK: &str =
        r#"{"choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;

    fn deployment(base: &str) -> Deployment {
        Deployment {
            headers: HeaderMap::new(),
            fallback: false,
            idx: DeploymentIdx(0),
            id: "freebuff".into(),
            provider: ProviderKind(provider::FREEBUFF.into()),
            credential: "key".into(),
            base_url: format!("{base}/api/v1/").parse().unwrap(),
            upstream_model: MODEL.into(),
            strip_request_fields: Box::default(),
            priority: 0,
            quota: QuotaSpec::default(),
            timeout: Duration::from_secs(30),
            max_context: None,
            expires_at: None,
            billing: Billing::Billed,
        }
    }

    fn request(format: InboundFormat, stream: bool, body: Value) -> GatewayRequest {
        GatewayRequest {
            format,
            model: "freebuff/deepseek-v4-flash".into(),
            stream,
            body: Bytes::from(body.to_string()),
            anthropic_headers: HeaderMap::new(),
            est_input_tokens: 1,
            max_output_tokens: None,
            context_overflow: None,
            request_id: RequestId("req".into()),
            end_user: Some("synthetic-user-hash".into()),
            client_key: None,
            image_count: 0,
            free_only: false,
        }
    }

    async fn upstream() -> TestServer {
        TestServer::start(Router::new().route(
            "/api/v1/chat/completions",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers[header::AUTHORIZATION], "Bearer synthetic-freebuff");
                assert_eq!(body["model"], MODEL);
                assert!(body.get("user").is_none());
                assert!(body.get("input").is_none());
                assert_eq!(body["messages"][0]["role"], "user");
                match body["messages"][0]["content"].as_str().unwrap() {
                    "limited" => (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, "7")],
                        Json(json!({"error":"free_mode_capacity_deferred"})),
                    )
                        .into_response(),
                    "stream" => {
                        assert_eq!(body["stream"], true);
                        assert_eq!(headers[header::ACCEPT], "text/event-stream");
                        (
                            [(header::CONTENT_TYPE, "text/event-stream")],
                            format!("data: {CHUNK}\n\ndata: [DONE]\n\n"),
                        )
                            .into_response()
                    }
                    "complete" => {
                        assert_eq!(headers[header::ACCEPT], "application/json");
                        Json(json!({
                            "id":"chatcmpl-freebuff", "object":"chat.completion", "model":MODEL,
                            "choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],
                            "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3},
                        }))
                        .into_response()
                    }
                    _ => panic!("unexpected prompt"),
                }
            }),
        ))
        .await
        .unwrap()
    }

    async fn exchange(
        server: &TestServer,
        req: &GatewayRequest,
    ) -> Result<GatewayResponse, UpstreamError> {
        let adapter = Freebuff::new();
        assert_eq!(adapter.kind().0.as_ref(), "freebuff");
        let dep = deployment(&server.url());
        let client = Client::builder(TokioExecutor::new()).build_http();
        let outgoing = adapter
            .build_request(
                req,
                &dep,
                &AuthMaterial::Bearer("synthetic-freebuff".into()),
            )
            .await
            .unwrap();
        assert!(outgoing.headers()[header::AUTHORIZATION].is_sensitive());
        let response = client.request(outgoing.map(Full::new)).await.unwrap();
        adapter.parse_response(req, &dep, response).await
    }

    #[tokio::test]
    async fn chat_passthrough_and_converted_completions() {
        let server = upstream().await;
        let messages = json!([{"role":"user","content":"complete"}]);
        for (format, body, support, object, kind, content) in [
            (
                InboundFormat::OpenAiChat,
                json!({"model":"alias","messages":messages}),
                Support::Passthrough,
                "/object",
                "chat.completion",
                "/choices/0/message/content",
            ),
            (
                InboundFormat::AnthropicMessages,
                json!({"model":"alias","messages":messages,"max_tokens":32}),
                Support::Convert,
                "/type",
                "message",
                "/content/0/text",
            ),
            (
                InboundFormat::OpenAiResponses,
                json!({"model":"alias","input":"complete","max_output_tokens":32}),
                Support::Convert,
                "/object",
                "response",
                "/output/0/content/0/text",
            ),
        ] {
            assert_eq!(Freebuff::new().supports(format), support);
            let req = request(format, false, body);
            let GatewayResponse::Complete { status, body, .. } =
                exchange(&server, &req).await.unwrap()
            else {
                panic!("expected a complete response");
            };
            assert_eq!(status, StatusCode::OK);
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["model"], MODEL);
            assert_eq!(body.pointer(object).and_then(Value::as_str), Some(kind));
            assert_eq!(body.pointer(content).and_then(Value::as_str), Some("Hello"));
        }
    }

    #[tokio::test]
    async fn chat_sse_passes_through() {
        let server = upstream().await;
        let req = request(
            InboundFormat::OpenAiChat,
            true,
            json!({"model":"alias","messages":[{"role":"user","content":"stream"}],"stream":true}),
        );
        let GatewayResponse::Stream(mut stream) = exchange(&server, &req).await.unwrap() else {
            panic!("expected a stream");
        };
        assert_eq!(stream.next().await.unwrap().unwrap().data, CHUNK);
        assert_eq!(stream.next().await.unwrap().unwrap().data, "[DONE]");
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn rate_limit_preserves_retry_after() {
        let server = upstream().await;
        let req = request(
            InboundFormat::OpenAiChat,
            false,
            json!({"model":"alias","messages":[{"role":"user","content":"limited"}]}),
        );
        assert!(matches!(
            exchange(&server, &req).await,
            Err(UpstreamError::RateLimited { retry_after: Some(delay) })
                if delay == Duration::from_secs(7)
        ));
    }
}
