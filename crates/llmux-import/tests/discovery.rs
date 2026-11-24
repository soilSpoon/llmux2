use axum::{
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::post,
    Router,
};
use llmux_import::discovery::{self, ProbeVerdict};
use llmux_testkit::TestServer;
use serde_json::{json, Value};

#[test]
fn experlabs_page_yields_free_promo_candidates() {
    let html = r#"{"model":"deepseek-v4.1-flash-uncensored-fast","promotion":"Free as deepseek-v4.1-flash-uncensored-fast:free"} another promo Free as gpt-7-omega:free&#x27;s end"#;
    let candidates = discovery::experlabs_promo_ids(html);
    assert_eq!(
        candidates.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        [
            "deepseek-v4.1-flash-uncensored-fast:free",
            "gpt-7-omega:free"
        ]
    );
    assert!(candidates.iter().all(|c| !c.evidence.is_empty()));
}

#[test]
fn experlabs_page_without_promos_yields_nothing() {
    assert!(
        discovery::experlabs_promo_ids("<html><body>No free rides here</body></html>").is_empty()
    );
}

#[tokio::test]
async fn probe_classifies_free_billed_and_gone() {
    let router = Router::new().route(
        "/v1/chat/completions",
        post(|headers: HeaderMap, body: Json<Value>| async move {
            assert_eq!(
                headers.get("authorization").unwrap(),
                "Bearer synthetic-key"
            );
            let model = body["model"].as_str().unwrap();
            match model {
                "free-model:free" => (
                    StatusCode::OK,
                    Json(json!({
                        "model": "free-model",
                        "choices": [{"message": {"content": "hi"}}],
                        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "cost": 0.0}
                    })),
                ),
                "paid-model" => (
                    StatusCode::PAYMENT_REQUIRED,
                    Json(
                        json!({"error": {"message": "insufficient_credits", "code": "insufficient_credits"}}),
                    ),
                ),
                _ => (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": {"message": "model_not_allowed"}})),
                ),
            }
        }),
    );
    let server = TestServer::start(router).await.unwrap();
    let client = discovery::client().unwrap();
    let base = format!("{}/v1", server.url());

    assert!(matches!(
        discovery::probe_model(&client, &base, "synthetic-key", "free-model:free").await,
        ProbeVerdict::Free
    ));
    assert!(matches!(
        discovery::probe_model(&client, &base, "synthetic-key", "paid-model").await,
        ProbeVerdict::Billed
    ));
    assert!(matches!(
        discovery::probe_model(&client, &base, "synthetic-key", "gone-model").await,
        ProbeVerdict::Unavailable(_)
    ));
}
