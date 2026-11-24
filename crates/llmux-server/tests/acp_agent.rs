use axum::body::Bytes;
use http::{header, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};

/// A fake ACP CLI answering the JSON-RPC frames on stdin with one canned
/// `agent_message_chunk` and `end_turn`. The config's env sandbox points HOME
/// and the XDG dirs at the script's directory.
#[cfg(unix)]
fn fake_agent(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir.join(".local/share")).unwrap();
    std::fs::write(dir.join(".local/share/marker"), "").unwrap();
    let path = dir.join("agent");
    let body = r#"#!/bin/sh
test -f "$XDG_DATA_HOME/marker" || exit 3
while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  printf '%s\n' "$line" >> "$XDG_DATA_HOME/requests.log"
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id";;
    *'"session/new"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s1"}}\n' "$id";;
    *'"session/set_mode"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id";;
    *'"session/prompt"'*)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"agent says hi"}}}}'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id";;
  esac
done
"#
    .to_owned();
    let staged = dir.join("agent.tmp");
    std::fs::write(&staged, body).unwrap();
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&staged, &path).unwrap();
    path
}

#[cfg(unix)]
#[tokio::test]
async fn acp_agent_deployment_initializes_and_serves() {
    let dir = tempfile::tempdir().unwrap();
    let agent = fake_agent(dir.path());
    let client_key = dir.path().join("client");
    std::fs::write(&client_key, "synthetic-client").unwrap();
    let config = format!(
        r#"
[server]
client_keys = ["file:{}"]

[[credentials]]
id = "agent"
kind = "acp_agent"
provider = "acp_agent"
command = "{}"
args = ["--acp"]
home = "{}"
turn_timeout_ms = 10000

[[models]]
name = "cli"

[[models.deployments]]
id = "cli-agent"
credential = "agent"
upstream_model = "swe-1"
"#,
        client_key.display(),
        agent.display(),
        dir.path().display()
    );
    let runtime = RuntimeConfig::parse(&config).unwrap();
    assert_eq!(
        runtime.deployments[0].base_url,
        "http://127.0.0.1/acp-agent"
    );
    llmux_server::validate_startup(RuntimeConfig::parse(&config).unwrap())
        .expect("an acp_agent deployment must initialize");

    let gateway = TestServer::start(llmux_server::app(runtime).await.unwrap())
        .await
        .unwrap();
    let client = Client::builder(TokioExecutor::new()).build_http();
    let response = client
        .request(
            Request::post(format!("{}/v1/chat/completions", gateway.url()))
                .header(header::AUTHORIZATION, "Bearer synthetic-client")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(
                    json!({"model":"cli","messages":[{"role":"user","content":"hello"}]})
                        .to_string(),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-llmux-deployment"], "cli-agent");
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "agent says hi");
    let requests = std::fs::read_to_string(dir.path().join(".local/share/requests.log")).unwrap();
    assert!(requests.contains("session/prompt"), "{requests}");
    assert!(requests.contains("swe-1"), "{requests}");
}
