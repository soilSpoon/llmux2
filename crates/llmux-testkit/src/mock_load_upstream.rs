use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use std::{convert::Infallible, time::Duration};

const ROLE: &str = "data: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n";
const TOKEN: &str = "data: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n";
const STOP: &str = "data: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
const DONE: &str = "data: [DONE]\n\n";

#[derive(Clone, Copy)]
struct Mock {
    hold: bool,
    delay: Duration,
}

async fn chat(
    State(Mock { hold, delay }): State<Mock>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some("Bearer mock-backend")
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if request["model"] != "backend-model" {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    if request["stream"] != true {
        return Json(json!({
            "id": "mock",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "xxxxxxxx"}, "finish_reason": "stop"}]
        }))
        .into_response();
    }

    let events = stream::iter(
        std::iter::once(ROLE)
            .chain(std::iter::repeat_n(TOKEN, if hold { 1 } else { 8 }))
            .chain((!hold).then_some([STOP, DONE]).into_iter().flatten())
            .map(|event| Ok::<_, Infallible>(Bytes::from_static(event.as_bytes()))),
    );
    let body = if hold {
        Body::from_stream(events.chain(stream::pending()))
    } else {
        Body::from_stream(events)
    };
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

pub fn router(hold: bool, delay: Duration) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat))
        .with_state(Mock { hold, delay })
}
