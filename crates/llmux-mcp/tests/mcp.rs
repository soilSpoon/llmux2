use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

async fn admin() -> (TestServer, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let server = TestServer::start(
        Router::new()
            .route(
                "/admin/status",
                get(|| async { Json(json!({"uptime_ms": 1, "version": "test"})).into_response() }),
            )
            .route(
                "/admin/credentials/{id}/disabled",
                post(
                    |State(calls): State<Arc<AtomicUsize>>,
                     Path(id): Path<String>,
                     Json(body): Json<Value>| async move {
                        calls.fetch_add(1, Ordering::Relaxed);
                        assert_eq!(id, "key");
                        Json(json!({"id": id, "disabled": body["disabled"]}))
                    },
                ),
            )
            .route(
                "/admin/reload",
                post(|headers: http::HeaderMap| async move {
                    if headers["authorization"] != "Bearer admin-key" {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error":{"message":"denied"}})),
                        )
                            .into_response();
                    }
                    Json(json!({"reloaded": true})).into_response()
                }),
            )
            .with_state(counted),
    )
    .await
    .unwrap();
    (server, calls)
}

struct Mcp {
    child: Child,
}

impl Mcp {
    fn spawn(server: &TestServer) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_llmux-mcp"))
            .env("LLMUX_ADMIN_URL", server.url())
            .env("LLMUX_ADMIN_KEY", "admin-key")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self { child }
    }

    fn send(&mut self, request: Value) -> Value {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
        let mut reply = String::new();
        BufReader::new(self.child.stdout.as_mut().unwrap())
            .read_line(&mut reply)
            .unwrap();
        serde_json::from_str(&reply).unwrap()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[tokio::test]
async fn initialize_lists_tools_and_calls_status() {
    let (server, _) = admin().await;
    tokio::task::spawn_blocking(move || {
        let mut mcp = Mcp::spawn(&server);
        let reply = mcp.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
        assert_eq!(reply["result"]["serverInfo"]["name"], "llmux-mcp");

        let reply = mcp.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
        let tools = reply["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 6);
        assert!(tools.iter().any(|tool| tool["name"] == "llmux_status"));

        let reply = mcp.send(
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"llmux_status","arguments":{}}}),
        );
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        let body: Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["version"], "test");
        assert!(reply["result"].get("isError").is_none());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn set_disabled_posts_flag() {
    let (server, calls) = admin().await;
    tokio::task::spawn_blocking(move || {
        let mut mcp = Mcp::spawn(&server);
        mcp.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
        let reply = mcp.send(
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"llmux_set_disabled","arguments":{"id":"key","disabled":true}}}),
        );
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        let body: Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["disabled"], true);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn api_and_protocol_errors_are_reported() {
    let (server, _) = admin().await;
    tokio::task::spawn_blocking(move || {
        let mut mcp = Mcp::spawn(&server);
        mcp.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));

        // The server started without LLMUX_ADMIN_KEY? No — reload is the
        // only route the mock protects, and the key is set, so bad calls are:
        let reply = mcp.send(
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bogus_tool","arguments":{}}}),
        );
        assert_eq!(reply["result"]["isError"], true);

        let reply = mcp.send(
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"llmux_set_disabled","arguments":{"id":"key"}}}),
        );
        assert_eq!(reply["result"]["isError"], true);
        assert!(reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("disabled"));

        let reply = mcp.send(json!({"jsonrpc":"2.0","id":4,"method":"no/such"}));
        assert_eq!(reply["error"]["code"], -32601);
    })
    .await
    .unwrap();
}
