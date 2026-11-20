use axum::{body::Bytes, response::IntoResponse, routing::post, Json, Router};
use http::{header, HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use std::{
    fs,
    sync::{Arc, Mutex},
};

fn config(url: &str, dir: &std::path::Path, allowed: &str) -> String {
    for (file, value) in [
        ("owner", "owner-key"),
        ("other", "other-key"),
        ("refresh", "refresh"),
    ] {
        fs::write(dir.join(file), value).unwrap();
    }
    let dir = dir.display();
    format!(
        r#"
[server]
client_keys = ["file:{dir}/owner", "file:{dir}/other"]
[[credentials]]
id = "codex"
kind = "oauth"
provider = "openai_responses"
token_url = "{url}/token"
client_id = "issued"
refresh_token = "file:{dir}/refresh"
resource = "https://api.openai.com/v1"
[[models]]
name = "codex"
aliases = ["alias"]
{allowed}
[[models.deployments]]
id = "codex-upstream"
credential = "codex"
base_url = "{url}/v1"
upstream_model = "upstream"
"#
    )
}

fn completed() -> Value {
    json!({"id":"resp_test","object":"response","model":"upstream","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello"}]}],"usage":{"input_tokens":3,"output_tokens":2}})
}

async fn send(url: &str, path: &str, key: &str, body: Value) -> (StatusCode, String) {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let response = client
        .request(
            Request::post(format!("{url}/v1/{path}"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(body.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[test]
fn oauth_codex_requires_owner_keys() {
    let dir = tempfile::tempdir().unwrap();
    let text = config("http://127.0.0.1:1", dir.path(), "");
    let err = RuntimeConfig::parse(&text)
        .err()
        .expect("owner restriction required");
    assert!(err.to_string().contains("allowed_keys"));
    for allowed in [
        "allowed_keys = []".to_owned(),
        format!("allowed_keys = [\"file:{}/refresh\"]", dir.path().display()),
    ] {
        assert!(RuntimeConfig::parse(&config("http://127.0.0.1:1", dir.path(), &allowed)).is_err());
    }
    let allowed = format!("allowed_keys = [\"file:{}/owner\"]", dir.path().display());
    let text = config("http://127.0.0.1:1", dir.path(), &allowed)
        .replace("base_url = \"http://127.0.0.1:1/v1\"", "");
    let cfg = RuntimeConfig::parse(&text).unwrap();
    assert_eq!(cfg.deployments[0].base_url, "https://api.openai.com/v1");
}

async fn fixture(
    events: String,
    omit_content_type: bool,
) -> (
    tempfile::TempDir,
    TestServer,
    TestServer,
    Arc<Mutex<Vec<Value>>>,
) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let upstream = TestServer::start(
        Router::new()
            .route(
                "/token",
                post(|| async {
                    Json(json!({"access_token":"access","token_type":"Bearer","expires_in":3600}))
                }),
            )
            .route(
                "/v1/responses",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let seen = seen.clone();
                    let events = events.clone();
                    async move {
                        assert_eq!(headers[header::AUTHORIZATION], "Bearer access");
                        seen.lock().unwrap().push(body);
                        if omit_content_type {
                            http::Response::builder()
                                .body(events)
                                .unwrap()
                                .into_response()
                        } else {
                            ([(header::CONTENT_TYPE, "text/event-stream")], events).into_response()
                        }
                    }
                }),
            ),
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let allowed = format!("allowed_keys = [\"file:{}/owner\"]", dir.path().display());
    let app = llmux_server::app(
        RuntimeConfig::parse(&config(&upstream.url(), dir.path(), &allowed)).unwrap(),
    )
    .await
    .unwrap();
    let gateway = TestServer::start(app).await.unwrap();
    (dir, upstream, gateway, calls)
}

fn events() -> String {
    format!(
        "event: response.output_text.delta\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
        json!({"type":"response.output_text.delta","delta":"Hello","output_index":0,"content_index":0}),
        json!({"type":"response.completed","response":completed()})
    )
}

#[tokio::test]
async fn responses_normalization_aggregation_and_relay() {
    let (_dir, _upstream, gateway, calls) = fixture(events(), false).await;
    for stream in [false, true] {
        let body = json!({"model":"codex","input":"Hi","instructions":"Be brief","stream":stream,"store":true,"temperature":1,"top_p":1,"max_output_tokens":20,"metadata":{},"background":true,"user":"someone","moderation":{},"multi_agent":{},"prompt":{},"prompt_cache_retention":"24h","safety_identifier":"x","top_logprobs":2,"truncation":"auto","max_tool_calls":3});
        let (status, body) = send(&gateway.url(), "responses", "owner-key", body).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        if stream {
            assert!(body.contains("event: response.completed"));
            assert!(body.contains("Hello"));
        } else {
            assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), completed());
        }
    }
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    for body in calls.iter() {
        assert_eq!(
            body,
            &json!({"model":"upstream","input":[{"role":"user","content":"Hi"}],"instructions":"Be brief","stream":true,"store":false})
        );
    }
}

#[tokio::test]
async fn missing_upstream_content_type_is_accepted() {
    // The real Codex upstream (api.openai.com/v1/responses) returns its SSE
    // stream without a Content-Type header.
    let (_dir, _upstream, gateway, _) = fixture(events(), true).await;
    let (status, body) = send(
        &gateway.url(),
        "responses",
        "owner-key",
        json!({"model":"codex","input":"Hi"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), completed());
}

#[tokio::test]
async fn rejects_unsupported_inputs_and_owner_mismatch_without_upstream() {
    let (_dir, _upstream, gateway, calls) = fixture(events(), false).await;
    for model in ["codex", "alias"] {
        let (status, _) = send(
            &gateway.url(),
            "responses",
            "other-key",
            json!({"model":model,"input":"Hi"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    for extra in [
        json!({"previous_response_id":"resp_old"}),
        json!({"conversation":"old"}),
        json!({"input":[{"role":"system","content":"no"}]}),
        json!({"tools":[{"type":"file_search"}]}),
    ] {
        let mut body = json!({"model":"codex","input":[]});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert_eq!(
            send(&gateway.url(), "responses", "owner-key", body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn chat_roundtrip_and_images_tools() {
    let (_dir, _upstream, gateway, calls) = fixture(events(), false).await;
    for stream in [false, true] {
        let (status, body) = send(&gateway.url(), "chat/completions", "owner-key", json!({"model":"codex","stream":stream,"messages":[{"role":"system","content":"Be brief"},{"role":"user","content":[{"type":"text","text":"Hi"},{"type":"image_url","image_url":{"url":"https://example.com/a.png"}}]},{"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"test","arguments":"{}"}}]},{"role":"tool","tool_call_id":"c1","content":"ok"}],"tools":[{"type":"function","function":{"name":"test","parameters":{"type":"object"}}}]})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        if stream {
            assert!(body.contains("chat.completion.chunk"));
            assert!(body.contains("[DONE]"));
        } else {
            let value: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(value["choices"][0]["message"]["content"], "Hello");
            assert_eq!(value["usage"]["prompt_tokens"], 3);
        }
    }
    let calls = calls.lock().unwrap();
    for body in calls.iter() {
        assert_eq!(body["instructions"], "Be brief");
        assert_eq!(
            body["input"][0]["content"][1],
            json!({"type":"input_image","image_url":"https://example.com/a.png"})
        );
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["name"], "test");
    }
}

#[tokio::test]
async fn interrupted_and_failed_streams_are_not_success() {
    for events in [String::new(), "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\"}}}\n\n".into()] {
        let (_dir, _upstream, gateway, _) = fixture(events, false).await;
        assert_ne!(send(&gateway.url(), "responses", "owner-key", json!({"model":"codex","input":[]})).await.0, StatusCode::OK);
    }
}

#[tokio::test]
async fn owner_policy_covers_fallback_and_other_models() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let primary = Arc::new(AtomicUsize::new(0));
    let codex = Arc::new(AtomicUsize::new(0));
    let p = primary.clone();
    let c = codex.clone();
    let upstream = TestServer::start(
        Router::new()
            .route(
                "/token",
                post(|| async {
                    Json(json!({"access_token":"access","token_type":"Bearer","expires_in":3600}))
                }),
            )
            .route(
                "/v1/chat/completions",
                post(move || {
                    let p = p.clone();
                    async move {
                        p.fetch_add(1, Ordering::Relaxed);
                        (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "0")])
                    }
                }),
            )
            .route(
                "/v1/responses",
                post(move || {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, Ordering::Relaxed);
                        ([(header::CONTENT_TYPE, "text/event-stream")], events())
                    }
                }),
            ),
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let allowed = format!("allowed_keys = [\"file:{}/owner\"]", dir.path().display());
    let base = upstream.url();
    let path = dir.path().display();
    let text = config(&base, dir.path(), &allowed).replace(
        "[[models.deployments]]",
        &format!(
            r#"
[[models.deployments]]
id = "primary"
credential = "normal"
base_url = "{base}/v1"
upstream_model = "upstream"
priority = -1
[[models.deployments]]"#
        ),
    );
    let text = format!(
        r#"{text}
[[credentials]]
id = "normal"
kind = "api_key"
provider = "openai_compat"
key = "file:{path}/refresh"
[[models]]
name = "public"
[[models.deployments]]
id = "public-primary"
credential = "normal"
base_url = "{base}/v1"
upstream_model = "upstream"
"#
    );
    let gateway = TestServer::start(
        llmux_server::app(RuntimeConfig::parse(&text).unwrap())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        send(
            &gateway.url(),
            "responses",
            "other-key",
            json!({"model":"alias","input":"Hi"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(primary.load(Ordering::Relaxed), 0);
    assert_eq!(codex.load(Ordering::Relaxed), 0);
    assert_eq!(
        send(
            &gateway.url(),
            "responses",
            "other-key",
            json!({"model":"public","input":"Hi"})
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(codex.load(Ordering::Relaxed), 0);
    assert_eq!(
        send(
            &gateway.url(),
            "responses",
            "owner-key",
            json!({"model":"codex","input":"Hi"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(codex.load(Ordering::Relaxed), 1);
    assert_eq!(primary.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn http_auth_refresh_and_cooldown() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for status in [StatusCode::UNAUTHORIZED, StatusCode::TOO_MANY_REQUESTS] {
        let calls = Arc::new(AtomicUsize::new(0));
        let tokens = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let t = tokens.clone();
        let upstream = TestServer::start(Router::new()
            .route("/token", post(move || { let t = t.clone(); async move { t.fetch_add(1,Ordering::Relaxed); Json(json!({"access_token":"access","token_type":"Bearer","expires_in":3600})) } }))
            .route("/v1/responses",post(move || { let c = c.clone(); async move {
                let count = c.fetch_add(1,Ordering::Relaxed);
                if status == StatusCode::UNAUTHORIZED && count > 0 { (StatusCode::OK, [(header::CONTENT_TYPE,"text/event-stream")],events()) }
                else { (status,[(header::RETRY_AFTER,"60")],json!({"error":{"code":"insufficient_quota"}}).to_string()) }
            } }))).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let allowed = format!("allowed_keys = [\"file:{}/owner\"]", dir.path().display());
        let gateway = TestServer::start(
            llmux_server::app(
                RuntimeConfig::parse(&config(&upstream.url(), dir.path(), &allowed)).unwrap(),
            )
            .await
            .unwrap(),
        )
        .await
        .unwrap();
        for _ in 0..2 {
            let expected = if status == StatusCode::UNAUTHORIZED {
                StatusCode::OK
            } else {
                StatusCode::TOO_MANY_REQUESTS
            };
            assert_eq!(
                send(
                    &gateway.url(),
                    "responses",
                    "owner-key",
                    json!({"model":"codex","input":[]})
                )
                .await
                .0,
                expected
            );
        }
        assert_eq!(
            tokens.load(Ordering::Relaxed),
            if status == StatusCode::UNAUTHORIZED {
                2
            } else {
                1
            }
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            if status == StatusCode::UNAUTHORIZED {
                3
            } else {
                1
            }
        );
    }
}

#[tokio::test]
async fn function_output_converts_to_chat_json_and_sse() {
    let mut response = completed();
    response["output"] =
        json!([{"type":"function_call","call_id":"c1","name":"test","arguments":"{}"}]);
    let events = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\n",
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"c1","name":"test","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{}"}),
        json!({"type":"response.completed","response":response})
    );
    let (_dir, _upstream, gateway, _) = fixture(events, false).await;
    for stream in [false, true] {
        let (status, body) = send(
            &gateway.url(),
            "chat/completions",
            "owner-key",
            json!({"model":"codex","stream":stream,"messages":[]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        if stream {
            assert!(body.contains("tool_calls"));
            assert!(body.contains("[DONE]"));
        } else {
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
            assert_eq!(
                body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
                "{}"
            );
        }
    }
}

#[tokio::test]
async fn streaming_commits_before_completion_and_never_replays() {
    use axum::body::Body;
    use futures_util::stream;
    use std::{
        convert::Infallible,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    use tokio::{sync::Semaphore, time::timeout};
    for path in ["responses", "chat/completions"] {
        let gate = Arc::new(Semaphore::new(0));
        let hits = Arc::new(AtomicUsize::new(0));
        let g = gate.clone();
        let h = hits.clone();
        let upstream = TestServer::start(Router::new()
            .route("/token", post(|| async { Json(json!({"access_token":"access","token_type":"Bearer","expires_in":3600})) }))
            .route("/v1/responses",post(move || { let g = g.clone(); let h = h.clone(); async move {
                h.fetch_add(1, Ordering::Relaxed);
                let chunks = stream::unfold(0, move |i| { let g = g.clone(); async move {
                    let data = match i {
                        0 => format!("event: response.created\ndata: {}\n\n", json!({"type":"response.created","response":{"id":"resp_test","model":"upstream"}})),
                        1 => { g.acquire().await.unwrap().forget(); "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\"}}}\n\n".to_owned() },
                        _ => return None,
                    };
                    Some((Ok::<_, Infallible>(Bytes::from(data)),i+1))
                } });
                ([(header::CONTENT_TYPE,"text/event-stream")], Body::from_stream(chunks))
            } }))).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let allowed = format!("allowed_keys = [\"file:{}/owner\"]", dir.path().display());
        let text = format!(
            r#"{}
[[models.deployments]]
id = "backup"
credential = "codex"
base_url = "{}/v1"
upstream_model = "upstream"
priority = 1
"#,
            config(&upstream.url(), dir.path(), &allowed),
            upstream.url()
        );
        let gateway = TestServer::start(
            llmux_server::app(RuntimeConfig::parse(&text).unwrap())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let response = timeout(
            Duration::from_secs(2),
            client.request(
                Request::post(format!("{}/v1/{path}", gateway.url()))
                    .header(header::AUTHORIZATION, "Bearer owner-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Full::new(Bytes::from(
                        json!({"model":"codex","stream":true,"messages":[],"input":[]}).to_string(),
                    )))
                    .unwrap(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let first = timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        let first = String::from_utf8(first.to_vec()).unwrap();
        assert!(first.contains(if path == "responses" {
            "response.created"
        } else {
            "chat.completion.chunk"
        }));
        gate.add_permits(1);
        let tail = timeout(Duration::from_secs(2), body.collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        assert!(String::from_utf8(tail.to_vec())
            .unwrap()
            .contains("stream_broken"));
        assert_eq!(hits.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn chat_usage_is_opt_in_and_separate_from_finish() {
    let prefix = [
        json!({"type":"response.created","response":{"id":"resp_test","model":"upstream"}}),
        json!({"type":"response.refusal.delta","delta":"No"}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"c1","name":"test","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{}"}),
    ].iter().map(|value| format!("data: {value}\n\n")).collect::<String>();
    let (_dir, _upstream, gateway, _) = fixture(format!("{prefix}{}", events()), false).await;
    for include in [None, Some(false), Some(true)] {
        let mut request = json!({"model":"codex","stream":true,"messages":[]});
        if let Some(include) = include {
            request["stream_options"] = json!({"include_usage":include});
        }
        let (status, body) = send(&gateway.url(), "chat/completions", "owner-key", request).await;
        assert_eq!(status, StatusCode::OK);
        let chunks: Vec<Value> = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        let finish = chunks
            .iter()
            .position(|chunk| chunk["choices"][0]["finish_reason"] == "tool_calls")
            .unwrap();
        assert!(chunks[finish].get("usage").is_none_or(Value::is_null));
        for chunk in &chunks[..=finish] {
            if include == Some(true) {
                assert_eq!(chunk.get("usage"), Some(&Value::Null));
            } else {
                assert!(chunk.get("usage").is_none());
            }
        }
        assert_eq!(
            chunks.len(),
            finish + if include == Some(true) { 2 } else { 1 }
        );
        if include == Some(true) {
            assert_eq!(chunks[finish + 1]["choices"], json!([]));
            assert_eq!(
                chunks[finish + 1]["usage"],
                json!({"prompt_tokens":3,"completion_tokens":2,"total_tokens":5})
            );
        }
        assert!(body.trim_end().ends_with("data: [DONE]"));
    }
}
