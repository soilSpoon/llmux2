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
        let mut request = Request::builder()
            .method(method)
            .uri(format!("{}{}", self.url.trim_end_matches('/'), path))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(key) = &self.key {
            request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
        }
        let response = self
            .client
            .request(
                request
                    .body(Full::new(Bytes::from(body.to_string())))
                    .map_err(|err| err.to_string())?,
            )
            .await
            .map_err(|err| format!("request failed: {err}"))?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|err| format!("reading response failed: {err}"))?
            .to_bytes();
        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if status != StatusCode::OK {
            let detail = parsed["error"]["message"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
            return Err(format!("{status}: {detail}"));
        }
        Ok(parsed)
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
