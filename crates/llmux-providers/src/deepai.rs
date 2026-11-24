use crate::openai_compat::{bad_gateway, classify_error, collect, collect_error, endpoint_uri};
use async_trait::async_trait;
use bytes::Bytes;
use http::{header, HeaderMap, HeaderValue, Request, Response, StatusCode};
use hyper::body::Incoming;
use llmux_types::{
    header_request_id,
    provider::{DEEPAI as KIND_DEEPAI, DEEPAI_KEY_HEADER, DEEPAI_TEXT_MODELS},
    AuthMaterial, BuildError, Deployment, GatewayRequest, GatewayResponse, InboundFormat,
    ProviderAdapter, ProviderKind, Support, UpstreamError,
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct DeepAi(ProviderKind);

impl Default for DeepAi {
    fn default() -> Self {
        Self(ProviderKind(KIND_DEEPAI.into()))
    }
}

impl DeepAi {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ProviderAdapter for DeepAi {
    fn kind(&self) -> &ProviderKind {
        &self.0
    }

    /// The complete body is synthesized; headers (raw upstream headers, or the
    /// `x-request-id` parse_response adds for a real job id) carry the only
    /// trustworthy request id.
    fn upstream_request_id(&self, headers: &HeaderMap, _body: Option<&Bytes>) -> Option<Arc<str>> {
        header_request_id(headers)
    }

    fn supports(&self, inbound: InboundFormat) -> Support {
        match inbound {
            InboundFormat::OpenAiChat => Support::Convert,
            InboundFormat::OpenAiResponses | InboundFormat::AnthropicMessages => Support::No,
        }
    }

    async fn build_request(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        auth: &AuthMaterial,
    ) -> Result<Request<Bytes>, BuildError> {
        if self.supports(req.format) == Support::No {
            return Err(BuildError::UnsupportedFormat);
        }
        let model = crate::upstream_model(req, dep);
        if req.stream || !DEEPAI_TEXT_MODELS.contains(&model) {
            return Err(BuildError::UnsupportedConversion);
        }

        let text = prompt(&req.body)?;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("text", &text)
            .finish();
        let uri = endpoint_uri(&dep.base_url, &format!("/{model}"))?;
        let mut request = Request::post(uri).body(Bytes::from(body))?;
        let headers = request.headers_mut();
        headers.extend(dep.headers.clone());
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        let AuthMaterial::Header { name, value } = auth else {
            return Err(BuildError::InvalidAuth);
        };
        if name != DEEPAI_KEY_HEADER || value.is_empty() {
            return Err(BuildError::InvalidAuth);
        }
        let mut value = value.clone();
        value.set_sensitive(true);
        headers.insert(name.clone(), value);
        Ok(request)
    }

    async fn parse_response(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        resp: Response<Incoming>,
    ) -> Result<GatewayResponse, UpstreamError> {
        let (parts, body) = resp.into_parts();
        if parts.status == StatusCode::UNAUTHORIZED || parts.status == StatusCode::FORBIDDEN {
            return Err(UpstreamError::Auth { refreshable: false });
        }
        if !parts.status.is_success() {
            return Err(classify_error(
                parts.status,
                &parts.headers,
                collect_error(body).await,
                SystemTime::now(),
            ));
        }

        let bytes = collect(body).await?;
        let output: Value = serde_json::from_slice(&bytes).map_err(|_| bad_gateway())?;
        let text = output["output"].as_str().ok_or_else(bad_gateway)?;
        let id = output["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map_or_else(|| format!("chatcmpl-{}", req.request_id.0), str::to_owned);
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let completion = json!({
            "id": id,
            "object": "chat.completion",
            "created": created,
            "model": crate::upstream_model(req, dep),
            "choices": [{"index":0, "message":{"role":"assistant", "content":text}, "finish_reason":"stop"}],
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        // The complete body is synthesized; surface a real upstream job id
        // as a response header so upstream_request_id can record it without
        // mistaking the fallback id for a provider one. A real request-id
        // header on the upstream response stays authoritative, so skip the
        // stamped header then.
        if header_request_id(&parts.headers).is_none() {
            if let Some(job_id) = output["id"].as_str().filter(|id| !id.is_empty()) {
                if let Ok(value) = HeaderValue::from_str(job_id) {
                    headers.insert("x-request-id", value);
                }
            }
        }
        Ok(GatewayResponse::Complete {
            status: StatusCode::OK,
            headers,
            body: Bytes::from(completion.to_string()),
        })
    }
}

fn prompt(body: &[u8]) -> Result<String, BuildError> {
    let body: Value = serde_json::from_slice(body).map_err(|_| BuildError::InvalidBody)?;
    let fields = body.as_object().ok_or(BuildError::InvalidBody)?;
    if fields
        .keys()
        .any(|key| !matches!(key.as_str(), "model" | "messages" | "stream"))
    {
        return Err(BuildError::UnsupportedConversion);
    }
    let messages = body["messages"].as_array().ok_or(BuildError::InvalidBody)?;
    let [message] = messages.as_slice() else {
        return Err(BuildError::UnsupportedConversion);
    };
    let fields = message.as_object().ok_or(BuildError::InvalidBody)?;
    if message["role"] != "user"
        || fields
            .keys()
            .any(|key| !matches!(key.as_str(), "role" | "content"))
    {
        return Err(BuildError::UnsupportedConversion);
    }
    let content = &message["content"];
    let text = if let Some(text) = content.as_str() {
        text.to_owned()
    } else {
        let parts = content
            .as_array()
            .ok_or(BuildError::UnsupportedConversion)?;
        parts.iter().try_fold(String::new(), |mut text, part| {
            if part["type"] != "text" {
                return Err(BuildError::UnsupportedConversion);
            }
            text.push_str(part["text"].as_str().ok_or(BuildError::InvalidBody)?);
            Ok(text)
        })?
    };
    if text.trim().is_empty() {
        return Err(BuildError::InvalidBody);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::DeepAi;
    use bytes::Bytes;
    use http::{header, HeaderMap, HeaderValue, StatusCode};
    use http_body_util::Full;
    use hyper_util::{client::legacy::Client, rt::TokioExecutor};
    use llmux_testkit::TestServer;
    use llmux_types::{
        provider::DEEPAI_KEY_HEADER, AuthMaterial, Billing, BuildError, Deployment, DeploymentIdx,
        GatewayRequest, GatewayResponse, InboundFormat, ProviderAdapter, QuotaSpec, RequestId,
        Support, UpstreamError,
    };
    use serde_json::{json, Value};
    use std::time::Duration;

    #[tokio::test]
    async fn builds_text_model_request() {
        let adapter = DeepAi::new();
        let req = chat_request(json!({"messages":[{"role":"user","content":"Hello & 안녕"}]}));
        let dep = deployment("https://api.deepai.org/api/");
        let auth = api_key();

        let request = adapter.build_request(&req, &dep, &auth).await.unwrap();
        assert_eq!(request.method(), http::Method::POST);
        assert_eq!(request.uri(), "https://api.deepai.org/api/text-generator");
        assert_eq!(request.headers()["api-key"], "test-key");
        assert!(request.headers()["api-key"].is_sensitive());
        assert!(!request.headers().contains_key(header::AUTHORIZATION));
        assert_eq!(
            request.headers()[header::CONTENT_TYPE],
            "application/x-www-form-urlencoded"
        );
        assert_eq!(request.body(), "text=Hello+%26+%EC%95%88%EB%85%95");
    }

    #[tokio::test]
    async fn only_converts_supported_text_requests() {
        let adapter = DeepAi::new();
        let mut dep = deployment("https://api.deepai.org/api");
        let auth = api_key();
        assert_eq!(
            adapter.supports(InboundFormat::OpenAiChat),
            Support::Convert
        );
        assert_eq!(
            adapter.supports(InboundFormat::OpenAiResponses),
            Support::No
        );
        assert_eq!(
            adapter.supports(InboundFormat::AnthropicMessages),
            Support::No
        );

        let mut req = chat_request(json!({"messages":[{"role":"user","content":[
            {"type":"text","text":"one "},{"type":"text","text":"two"}
        ]}]}));
        dep.upstream_model = "*".into();
        req.model = "summarization".into();
        let request = adapter.build_request(&req, &dep, &auth).await.unwrap();
        assert_eq!(request.uri().path(), "/api/summarization");
        assert_eq!(request.body(), "text=one+two");

        for model in ["text2img", "../text-generator", "text-generator?x=1"] {
            req.model = model.into();
            assert!(matches!(
                adapter.build_request(&req, &dep, &auth).await,
                Err(BuildError::UnsupportedConversion)
            ));
        }

        dep.upstream_model = "text-generator".into();
        req.stream = true;
        assert!(matches!(
            adapter.build_request(&req, &dep, &auth).await,
            Err(BuildError::UnsupportedConversion)
        ));
        let message = json!({"role":"user","content":"hello"});
        for body in [
            json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.com/a.png"}}]}]}),
            json!({"messages":[{"role":"system","content":"Be brief"},message]}),
            json!({"messages":[message],"tools":[{"type":"function"}]}),
            json!({"messages":[message],"max_completion_tokens":5}),
            json!({"messages":[message],"n":2}),
        ] {
            assert!(matches!(
                adapter
                    .build_request(&chat_request(body), &dep, &auth)
                    .await,
                Err(BuildError::UnsupportedConversion)
            ));
        }
    }

    #[tokio::test]
    async fn translates_text_responses_and_classifies_errors() {
        use axum::{response::IntoResponse, routing::post, Json, Router};

        let server = TestServer::start(Router::new().route(
            "/api/text-generator",
            post(|body: String| async move {
                match body.as_str() {
                    "text=success" => {
                        Json(json!({"id":"job-123","output":"completed text"})).into_response()
                    }
                    "text=hdr" => (
                        [("x-request-id", "hdr_req_1")],
                        Json(json!({"id":"job-9","output":"done"})),
                    )
                        .into_response(),
                    "text=image" => {
                        Json(json!({"id":"job-123","output_url":"https://example.com/a.png"}))
                            .into_response()
                    }
                    "text=nontext" => {
                        Json(json!({"id":"job-123","output":["not a string"]})).into_response()
                    }
                    "text=invalid" => "not JSON".into_response(),
                    "text=unauthorized" => (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"status":"Please pass a valid Api-Key"})),
                    )
                        .into_response(),
                    "text=limited" => (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "7")])
                        .into_response(),
                    _ => panic!("unexpected request body"),
                }
            }),
        ))
        .await
        .unwrap();
        let adapter = DeepAi::new();
        let dep = deployment(&format!("{}/api", server.url()));
        let client = Client::builder(TokioExecutor::new()).build_http();
        for text in [
            "success",
            "hdr",
            "image",
            "nontext",
            "invalid",
            "unauthorized",
            "limited",
        ] {
            let req = chat_request(json!({"messages":[{"role":"user","content":text}]}));
            let outgoing = adapter.build_request(&req, &dep, &api_key()).await.unwrap();
            let incoming = client.request(outgoing.map(Full::new)).await.unwrap();
            let result = adapter.parse_response(&req, &dep, incoming).await;
            match text {
                "success" => {
                    let GatewayResponse::Complete {
                        status,
                        headers,
                        body,
                    } = result.unwrap()
                    else {
                        panic!("expected a complete response");
                    };
                    let completion: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(status, StatusCode::OK);
                    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
                    assert_eq!(completion["id"], "job-123");
                    assert_eq!(completion["model"], "text-generator");
                    assert_eq!(completion["object"], "chat.completion");
                    assert!(completion["created"].is_u64());
                    assert_eq!(
                        completion["choices"],
                        json!([{"index":0,"message":{
                        "role":"assistant","content":"completed text"
                    },"finish_reason":"stop"}])
                    );
                    assert!(completion.get("usage").is_none());
                    // The upstream job id must reach the request log even
                    // though the body is synthesized.
                    assert_eq!(
                        adapter
                            .upstream_request_id(&headers, Some(&body))
                            .as_deref(),
                        Some("job-123")
                    );
                }
                "hdr" => {
                    // A real upstream request-id header stays authoritative:
                    // the synthesized headers must not shadow it with the
                    // body's job id.
                    let GatewayResponse::Complete { headers, body, .. } = result.unwrap() else {
                        panic!("expected a complete response");
                    };
                    assert!(headers.get("x-request-id").is_none());
                    assert!(adapter.upstream_request_id(&headers, Some(&body)).is_none());
                }
                "unauthorized" => assert!(matches!(
                    result,
                    Err(UpstreamError::Auth { refreshable: false })
                )),
                "limited" => assert!(matches!(
                    result,
                    Err(UpstreamError::RateLimited { retry_after: Some(wait) }) if wait == Duration::from_secs(7)
                )),
                _ => assert!(matches!(
                    result,
                    Err(UpstreamError::Server {
                        status: StatusCode::BAD_GATEWAY
                    })
                )),
            }
        }
    }

    fn api_key() -> AuthMaterial {
        AuthMaterial::Header {
            name: DEEPAI_KEY_HEADER.parse().unwrap(),
            value: HeaderValue::from_static("test-key"),
        }
    }

    fn chat_request(body: Value) -> GatewayRequest {
        GatewayRequest {
            format: InboundFormat::OpenAiChat,
            model: "public-model".into(),
            stream: false,
            body: Bytes::from(body.to_string()),
            anthropic_headers: HeaderMap::new(),
            est_input_tokens: 0,
            max_output_tokens: None,
            context_overflow: None,
            request_id: RequestId("r".into()),
            end_user: None,
            client_key: None,
            image_count: 0,
            free_only: false,
        }
    }

    fn deployment(base_url: &str) -> Deployment {
        Deployment {
            headers: HeaderMap::new(),
            fallback: false,
            idx: DeploymentIdx(0),
            id: "deepai-test".into(),
            provider: DeepAi::new().kind().clone(),
            credential: "key".into(),
            base_url: base_url.parse().unwrap(),
            upstream_model: "text-generator".into(),
            strip_request_fields: Box::default(),
            priority: 0,
            quota: QuotaSpec::default(),
            timeout: Duration::from_secs(10),
            max_context: None,
            expires_at: None,
            priority_boost: None,
            priority_boost_until: None,
            billing: Billing::Billed,
        }
    }
}
