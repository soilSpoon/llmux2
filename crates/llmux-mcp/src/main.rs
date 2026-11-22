//! `llmux-mcp` — stdio MCP server exposing the llmux2 admin API as tools.
//! Thin client only: every tool calls `/admin` like the `llmux` CLI.

use bytes::Bytes;
use http::{header, Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL: &str = "2025-06-18";

fn tools() -> Value {
    json!([
        {"name": "llmux_status", "description": "Gateway uptime, version and response counters", "inputSchema": {"type": "object", "properties": {}}},
        {"name": "llmux_deployments", "description": "Deployments with model, provider and live routing stats", "inputSchema": {"type": "object", "properties": {}}},
        {"name": "llmux_credentials", "description": "Configured credentials: kind, provider, disabled flag and quota (never secrets)", "inputSchema": {"type": "object", "properties": {}}},
        {"name": "llmux_models", "description": "Canonical models and their deployment order", "inputSchema": {"type": "object", "properties": {}}},
        {"name": "llmux_set_disabled", "description": "Enable or disable all deployments drawing on a credential", "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}, "disabled": {"type": "boolean"}}, "required": ["id", "disabled"]}},
        {"name": "llmux_reload", "description": "Reload the config file and atomically swap routing; reports restart-only changes as warnings", "inputSchema": {"type": "object", "properties": {}}},
    ])
}

struct Admin {
    url: String,
    key: Option<String>,
    client: Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        Full<Bytes>,
    >,
}

impl Admin {
    fn new() -> Self {
        let connector = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        Self {
            url: std::env::var("LLMUX_ADMIN_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
            key: std::env::var("LLMUX_ADMIN_KEY").ok(),
            client: Client::builder(TokioExecutor::new()).build(connector),
        }
    }

    async fn call(&self, method: Method, path: &str, body: Value) -> Result<Value, String> {
        let parsed =
            url::Url::parse(&self.url).map_err(|err| format!("bad LLMUX_ADMIN_URL: {err}"))?;
        // 127.0.0.0/8 and ::1 are all loopback, not just 127.0.0.1.
        let loopback = match parsed.host() {
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            Some(url::Host::Domain(domain)) => domain == "localhost",
            None => false,
        };
        if parsed.scheme() == "http" && !loopback && self.key.is_some() {
            return Err(
                "refusing to send the admin key over plaintext http to a non-loopback host".into(),
            );
        }
        let mut request = Request::builder()
            .method(method)
            .uri(format!("{}{}", self.url.trim_end_matches('/'), path))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(key) = &self.key {
            request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
        }
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.client.request(
                request
                    .body(Full::new(Bytes::from(body.to_string())))
                    .map_err(|err| err.to_string())?,
            ),
        )
        .await
        .map_err(|_| "request timed out after 30s".to_string())?
        .map_err(|err| format!("request failed: {err}"))?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|err| format!("reading response failed: {err}"))?
            .to_bytes();
        if status != StatusCode::OK {
            let detail = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|body| body["error"]["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
            return Err(format!("{status}: {detail}"));
        }
        serde_json::from_slice(&body).map_err(|err| format!("invalid JSON response: {err}"))
    }
}

async fn call_tool(admin: &Admin, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "llmux_status" => admin.call(Method::GET, "/admin/status", json!({})).await,
        "llmux_deployments" => {
            admin
                .call(Method::GET, "/admin/deployments", json!({}))
                .await
        }
        "llmux_credentials" => {
            admin
                .call(Method::GET, "/admin/credentials", json!({}))
                .await
        }
        "llmux_models" => admin.call(Method::GET, "/admin/models", json!({})).await,
        "llmux_reload" => admin.call(Method::POST, "/admin/reload", json!({})).await,
        "llmux_set_disabled" => {
            let id = args["id"].as_str().ok_or("missing string argument `id`")?;
            // Percent-encode: config ids may contain `/`, `%` etc. and the
            // admin handler decodes the segment before lookup.
            let mut encoded = String::new();
            for byte in id.bytes() {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                    encoded.push(byte as char);
                } else {
                    encoded.push_str(&format!("%{byte:02X}"));
                }
            }
            let id = encoded;
            let disabled = args["disabled"]
                .as_bool()
                .ok_or("missing boolean argument `disabled`")?;
            admin
                .call(
                    Method::POST,
                    &format!("/admin/credentials/{id}/disabled"),
                    json!({"disabled": disabled}),
                )
                .await
        }
        _ => Err(format!("unknown tool `{name}`")),
    }
}

fn result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: &Value, code: i64, message: impl std::fmt::Display) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.to_string()}})
}

async fn handle(admin: &Admin, request: &Value) -> Option<Value> {
    let method = request["method"].as_str()?;
    let id = &request["id"];
    if method.starts_with("notifications/") {
        return None;
    }
    if id.is_null() {
        return None;
    }
    Some(match method {
        "initialize" => result(
            id,
            json!({
                "protocolVersion": PROTOCOL,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "llmux-mcp", "version": env!("CARGO_PKG_VERSION")},
            }),
        ),
        "ping" => result(id, json!({})),
        "tools/list" => result(id, json!({"tools": tools()})),
        "tools/call" => {
            let name = request["params"]["name"].as_str().unwrap_or_default();
            let args = &request["params"]["arguments"];
            match call_tool(admin, name, args).await {
                Ok(body) => result(
                    id,
                    json!({"content": [{"type": "text", "text": serde_json::to_string_pretty(&body).unwrap()}]}),
                ),
                Err(err) => result(
                    id,
                    json!({"content": [{"type": "text", "text": err}], "isError": true}),
                ),
            }
        }
        _ => error(id, -32601, format!("method not found: {method}")),
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let admin = Admin::new();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(request) => handle(&admin, &request).await,
            Err(_) => Some(error(&Value::Null, -32700, "parse error")),
        };
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}").expect("stdout");
            stdout.flush().expect("stdout");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Path, State},
        http::StatusCode,
        response::IntoResponse,
        routing::{get, post},
        Json, Router,
    };
    use llmux_testkit::TestServer;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    async fn admin() -> (Admin, TestServer, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let server = TestServer::start(
            Router::new()
                .route(
                    "/admin/status",
                    get(|| async { Json(json!({"uptime_ms": 1, "version": "test"})) }),
                )
                .route(
                    "/admin/deployments",
                    get(|| async { Json(json!({"deployments": []})) }),
                )
                .route(
                    "/admin/credentials",
                    get(|| async { Json(json!({"credentials": []})) }),
                )
                .route(
                    "/admin/models",
                    get(|| async { Json(json!({"models": []})) }),
                )
                .route(
                    "/admin/credentials/{id}/disabled",
                    post(
                        |State(calls): State<Arc<AtomicUsize>>,
                         Path(id): Path<String>,
                         Json(body): Json<Value>| async move {
                            calls.fetch_add(1, Ordering::Relaxed);
                            assert_eq!(id, "a/b%c");
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
        let admin = Admin {
            url: server.url(),
            key: Some("admin-key".into()),
            client: Client::builder(TokioExecutor::new()).build(
                HttpsConnectorBuilder::new()
                    .with_webpki_roots()
                    .https_or_http()
                    .enable_http1()
                    .build(),
            ),
        };
        (admin, server, calls)
    }

    #[tokio::test]
    async fn tools_forward_to_admin_routes() {
        let (admin, _server, calls) = admin().await;
        for tool in [
            "llmux_status",
            "llmux_deployments",
            "llmux_credentials",
            "llmux_models",
            "llmux_reload",
        ] {
            let reply = handle(
                &admin,
                &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":{}}}),
            )
            .await
            .unwrap();
            assert!(reply["result"].get("isError").is_none(), "{tool}: {reply}");
        }
        let reply = handle(
            &admin,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"llmux_set_disabled","arguments":{"id":"a/b%c","disabled":true}}}),
        )
        .await
        .unwrap();
        assert!(reply["result"].get("isError").is_none(), "{reply}");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn protocol_and_error_paths() {
        let (admin, _server, _calls) = admin().await;
        let reply = handle(
            &admin,
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        )
        .await
        .unwrap();
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL);

        let reply = handle(&admin, &json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
            .await
            .unwrap();
        assert_eq!(reply["result"], json!({}));

        let reply = handle(
            &admin,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await
        .unwrap();
        assert_eq!(reply["result"]["tools"].as_array().unwrap().len(), 6);

        let reply = handle(
            &admin,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
        assert!(reply.is_none());
        let reply = handle(&admin, &json!({"jsonrpc":"2.0","method":"ping"})).await;
        assert!(reply.is_none());

        let reply = handle(&admin, &json!({"jsonrpc":"2.0","id":1,"method":"no/such"}))
            .await
            .unwrap();
        assert_eq!(reply["error"]["code"], -32601);

        let reply = handle(
            &admin,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"bogus","arguments":{}}}),
        )
        .await
        .unwrap();
        assert_eq!(reply["result"]["isError"], true);

        // Auth failure on the admin side becomes isError, not a crash.
        let denied = Admin {
            url: admin.url.clone(),
            key: Some("wrong".into()),
            client: admin.client.clone(),
        };
        let reply = handle(
            &denied,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"llmux_reload","arguments":{}}}),
        )
        .await
        .unwrap();
        assert_eq!(reply["result"]["isError"], true);
        assert!(reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("denied"));

        // Plaintext http to a non-loopback host is refused before any request.
        let remote = Admin {
            url: "http://example.com".into(),
            ..denied
        };
        let reply = handle(
            &remote,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"llmux_status","arguments":{}}}),
        )
        .await
        .unwrap();
        assert_eq!(reply["result"]["isError"], true);
        assert!(reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("plaintext"));
    }
}
