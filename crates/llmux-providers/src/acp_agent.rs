//! Generic agent-CLI bridge: serves an OpenAI chat request by driving an
//! agent CLI child — `auggie --acp`, `devin acp`, and any other CLI speaking
//! ACP JSON-RPC on stdio. The credential's [`AcpAgentSpec`] names the command,
//! args, environment allowlist, sandbox home, turn timeout, and whether the
//! child is spawned per request or kept persistent.
//!
//! App-server CLIs that speak a different JSON-RPC dialect (Codex
//! `app-server`, zcode — `thread/start`/`turn/start`/`item/agentMessage/delta`)
//! are a separate protocol family and do not go through this adapter.

use crate::acp::{
    cli_command, completion as acp_completion, labelled_turns, stage_images, usage_body,
    AbortOnDrop, AcpSession, ImageStaging,
};
use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use llmux_types::{
    header_request_id, provider, AcpAgentSpec, AcpSessionMode, AuthError, AuthMaterial, BuildError,
    CredentialSource, Deployment, GatewayRequest, GatewayResponse, InboundFormat, ProviderAdapter,
    ProviderKind, SseEvent, Support, UpstreamError,
};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
use tokio::{sync::Mutex, time::timeout};

/// `acp_agent` credentials describe a local process, not remote auth — this
/// source exists so deployments referencing the credential pass the executor's
/// credential lookup; it contributes no auth material.
pub struct NoAuthSource;

#[async_trait]
impl CredentialSource for NoAuthSource {
    fn current(&self) -> Result<AuthMaterial, AuthError> {
        Ok(AuthMaterial::None)
    }

    async fn refresh(&self) -> Result<(), AuthError> {
        Ok(())
    }
}

/// Pins the bridge contract so the reply is one assistant message and nothing
/// else — the same shape `devin_acp` enforces, without the Devin name.
const FRAME: &str = "[Chat Bridge]\nA conversation transcript follows under [Conversation]. Write the next assistant reply to it.\nReply with only that reply: plain text, no commentary about the conversation, no Markdown fences, and never use your own tools.\n";

/// Flattens chat messages into one labelled transcript under `FRAME`; client
/// `tools` are not forwarded — the agent answers text only.
fn prompt_text(messages: &[Value]) -> Option<String> {
    let turns = labelled_turns(messages);
    (!turns.is_empty()).then(|| {
        let transcript = turns
            .iter()
            .map(|(role, text)| format!("{role}: {text}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        format!("{FRAME}\n\n[Conversation]\n{transcript}")
    })
}

/// Whether a failed turn may keep a persistent child: only a well-formed RPC
/// error response that ends the turn definitively — anything else leaves the
/// child's in-flight state unknown, so it is dropped and lazily respawned.
fn reusable_after(err: &UpstreamError) -> bool {
    matches!(
        err,
        UpstreamError::RateLimited { .. }
            | UpstreamError::QuotaExhausted { .. }
            | UpstreamError::ModelNotFound { .. }
    )
}

pub struct AcpAgent {
    kind: ProviderKind,
    /// The process spec per credential id; a deployment's `credential` selects it.
    specs: Arc<HashMap<Arc<str>, AcpAgentSpec>>,
    /// Persistent children keyed by credential id. The lock is held for a whole
    /// turn, serializing requests through the one connection; a turn takes the
    /// session out of the map so cancelling or failing it drops (and kills)
    /// the child instead of leaving it mid-turn for the next request.
    sessions: Arc<Mutex<HashMap<Arc<str>, AcpSession>>>,
}

impl AcpAgent {
    pub fn new(specs: HashMap<Arc<str>, AcpAgentSpec>) -> Self {
        Self {
            kind: ProviderKind(provider::ACP_AGENT.into()),
            specs: Arc::new(specs),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// One turn against the credential's child. For `per_request` the child is
    /// spawned, spoken to, and killed inside `turn_timeout`; for `persistent`
    /// an already-initialized child is reused (or respawned when it died).
    async fn run_turn(
        sessions: &Mutex<HashMap<Arc<str>, AcpSession>>,
        credential: &Arc<str>,
        spec: &AcpAgentSpec,
        model: &str,
        prompt: &[Value],
        chunks: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<(String, Option<Value>), UpstreamError> {
        let cwd = spec.cwd().to_string_lossy().into_owned();
        timeout(spec.turn_timeout, async {
            match spec.mode {
                AcpSessionMode::PerRequest => {
                    let mut session = spawn_ready(spec).await?;
                    session.set_chunks(chunks);
                    let out = session.chat_turn(&cwd, model, prompt).await;
                    session.kill();
                    out
                }
                AcpSessionMode::Persistent => {
                    // The session is checked out of the map for the turn so
                    // the map lock is never held across a spawn or turn —
                    // concurrent turns on other credentials do not serialize.
                    let removed = sessions.lock().await.remove(credential);
                    let mut session = match removed {
                        // A dead child is dropped (kill_on_drop) and respawned
                        // lazily rather than failing this request.
                        Some(mut session) => {
                            if session.exited() {
                                spawn_ready(spec).await?
                            } else {
                                session
                            }
                        }
                        None => spawn_ready(spec).await?,
                    };
                    session.set_chunks(chunks);
                    match session.chat_turn(&cwd, model, prompt).await {
                        Ok(out) => {
                            // The stream sender must be dropped before caching:
                            // a session cached with a live sender holds the
                            // response stream open after its turn completes.
                            session.set_chunks(None);
                            sessions.lock().await.insert(credential.clone(), session);
                            Ok(out)
                        }
                        Err(err) => {
                            if reusable_after(&err) {
                                session.set_chunks(None);
                                sessions.lock().await.insert(credential.clone(), session);
                            }
                            Err(err)
                        }
                    }
                }
            }
        })
        .await
        .map_err(|_| UpstreamError::Timeout)?
    }

    /// Streams one SSE delta per ACP `agent_message_chunk` as the child emits
    /// it. Errors before the first event surface as the first stream item so
    /// the executor can fail over.
    fn stream(
        &self,
        spec: AcpAgentSpec,
        credential: Arc<str>,
        model: String,
        prompt: Vec<Value>,
        include_usage: bool,
        // Held until the stream ends: the staged image files must outlive the
        // turn, including an aborted one — the guard's locals drop with it.
        _staged: Option<ImageStaging>,
    ) -> GatewayResponse {
        let sessions = self.sessions.clone();
        GatewayResponse::Stream(Box::pin(async_stream::stream! {
            let _staged = _staged;
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let mut task = {
                let model = model.clone();
                AbortOnDrop(tokio::spawn(async move {
                    Self::run_turn(&sessions, &credential, &spec, &model, &prompt, Some(tx)).await
                }))
            };
            let delta = |text: String| {
                let mut chunk = acp_completion("acp", &model, &text, true);
                chunk["choices"][0]["delta"] = json!({"content":text});
                Ok(SseEvent { event: None, data: chunk.to_string() })
            };
            // The role-only delta mirrors the first chunk OpenAI servers send;
            // it goes out on the first session/update marker, never ahead of an error.
            let mut announced = false;
            while let Some(text) = rx.recv().await {
                if !announced {
                    announced = true;
                    let mut chunk = acp_completion("acp", &model, "", true);
                    chunk["choices"][0]["delta"] = json!({"role":"assistant","content":""});
                    yield Ok(SseEvent { event: None, data: chunk.to_string() });
                }
                if !text.is_empty() {
                    yield delta(text);
                }
            }
            let usage = match (&mut task.0).await {
                Ok(Ok((_, usage))) => usage,
                Ok(Err(err)) => {
                    yield Err(err);
                    return;
                }
                Err(_) => {
                    yield Err(UpstreamError::StreamBroken { bytes_sent: 0 });
                    return;
                }
            };
            let mut done = acp_completion("acp", &model, "", true);
            done["choices"][0] = json!({"index":0,"delta":{},"finish_reason":"stop"});
            yield Ok(SseEvent { event: None, data: done.to_string() });
            if include_usage {
                if let Some(usage) = usage_body(usage) {
                    let mut chunk = acp_completion("acp", &model, "", true);
                    chunk["choices"] = json!([]);
                    chunk["usage"] = usage;
                    yield Ok(SseEvent { event: None, data: chunk.to_string() });
                }
            }
            yield Ok(SseEvent { event: None, data: "[DONE]".into() });
        }))
    }

    async fn serve(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
    ) -> Result<GatewayResponse, UpstreamError> {
        let spec = self
            .specs
            .get(&dep.credential)
            .ok_or(UpstreamError::Server {
                status: StatusCode::BAD_GATEWAY,
            })?;
        let body: Value =
            serde_json::from_slice(&req.body).map_err(|_| UpstreamError::BadRequest {
                status: StatusCode::BAD_REQUEST,
                body: Bytes::new(),
            })?;
        let mut prompt = prompt_text(body["messages"].as_array().map_or(&[][..], |m| m)).ok_or(
            UpstreamError::BadRequest {
                status: StatusCode::BAD_REQUEST,
                body: Bytes::new(),
            },
        )?;
        let (_staged, entries) = stage_images(
            &spec.home,
            body["messages"].as_array().map_or(&[][..], |m| m),
        )?;
        if !entries.is_empty() {
            prompt.push_str("\n\n[Images]");
            for (index, entry) in entries.iter().enumerate() {
                prompt.push_str(&format!("\nimage {}: {entry}", index + 1));
            }
            prompt.push_str(
                "\n\nYou may read the files and URLs listed under [Images] to see the \
                 attached images; no other tool use is needed for this reply.",
            );
        }
        let prompt = vec![json!({"type":"text","text":prompt})];
        let model = crate::upstream_model(req, dep).to_owned();
        if req.stream {
            let include_usage = body["stream_options"]["include_usage"] == json!(true);
            return Ok(self.stream(
                spec.clone(),
                dep.credential.clone(),
                model,
                prompt,
                include_usage,
                _staged,
            ));
        }
        let (text, usage) =
            Self::run_turn(&self.sessions, &dep.credential, spec, &model, &prompt, None).await?;
        let mut reply = acp_completion("acp", &model, text.trim(), false);
        if let Some(usage) = usage_body(usage) {
            reply["usage"] = usage;
        }
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        Ok(GatewayResponse::Complete {
            status: StatusCode::OK,
            headers,
            body: Bytes::from(reply.to_string()),
        })
    }
}

/// Spawns the child and completes the ACP handshake (`initialize` +
/// `session/set_mode`). Failure surfaces as a connect/stream error so the
/// caller maps it to a failover.
async fn spawn_ready(spec: &AcpAgentSpec) -> Result<AcpSession, UpstreamError> {
    let mut session = AcpSession::spawn(&mut acp_command(spec))?;
    session.handshake().await?;
    Ok(session)
}

fn acp_command(spec: &AcpAgentSpec) -> tokio::process::Command {
    cli_command(
        &spec.command,
        &spec.args,
        &spec.home,
        spec.cwd.as_deref(),
        &spec.env,
    )
}

#[async_trait]
impl ProviderAdapter for AcpAgent {
    fn kind(&self) -> &ProviderKind {
        &self.kind
    }

    /// Complete bodies are synthesized locally (`chatcmpl-acp-{secs}` ids are
    /// gateway-generated and not unique per request); only headers could carry
    /// a real request id.
    fn upstream_request_id(&self, headers: &HeaderMap, _body: Option<&Bytes>) -> Option<Arc<str>> {
        header_request_id(headers)
    }

    fn supports(&self, inbound: InboundFormat) -> Support {
        match inbound {
            InboundFormat::OpenAiChat => Support::Passthrough,
            _ => Support::No,
        }
    }

    /// The executor requires a request; this one is never sent because `invoke` answers.
    async fn build_request(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        _auth: &AuthMaterial,
    ) -> Result<Request<Bytes>, BuildError> {
        if req.format != InboundFormat::OpenAiChat {
            return Err(BuildError::UnsupportedFormat);
        }
        Ok(Request::builder()
            .method(Method::POST)
            .uri(dep.base_url.clone())
            .body(Bytes::new())?)
    }

    async fn parse_response(
        &self,
        _req: &GatewayRequest,
        _dep: &Deployment,
        _resp: Response<Incoming>,
    ) -> Result<GatewayResponse, UpstreamError> {
        Err(UpstreamError::Server {
            status: StatusCode::BAD_GATEWAY,
        })
    }

    async fn invoke(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
    ) -> Option<Result<GatewayResponse, UpstreamError>> {
        Some(self.serve(req, dep).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_is_framed_and_history_is_labelled() {
        let msgs = [json!({"role":"user","content":"hi"})];
        assert_eq!(
            prompt_text(&msgs).unwrap(),
            format!("{FRAME}\n\n[Conversation]\nuser: hi")
        );
        let many = [
            json!({"role":"system","content":"be brief"}),
            json!({"role":"user","content":[{"type":"text","text":"hi"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]}),
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"c1","content":"a1"}),
        ];
        let prompt = prompt_text(&many).unwrap();
        assert!(prompt.contains("system: be brief"), "{prompt}");
        assert!(prompt.contains("user: hi\n[image 1]"), "{prompt}");
        assert!(prompt.contains("assistant: [tool_calls:"), "{prompt}");
        assert!(prompt.contains("tool(c1): a1"), "{prompt}");
        assert!(prompt_text(&[json!({"role":"user","content":""})]).is_none());
    }

    #[test]
    fn only_definitive_rpc_failures_keep_a_persistent_child() {
        for err in [
            UpstreamError::RateLimited { retry_after: None },
            UpstreamError::QuotaExhausted { reset_at: None },
            UpstreamError::ModelNotFound {
                status: StatusCode::NOT_FOUND,
                body: Bytes::new(),
            },
        ] {
            assert!(reusable_after(&err), "{err}");
        }
        for err in [
            UpstreamError::Connect,
            UpstreamError::Timeout,
            UpstreamError::StreamBroken { bytes_sent: 0 },
            UpstreamError::Server {
                status: StatusCode::BAD_GATEWAY,
            },
        ] {
            assert!(!reusable_after(&err), "{err}");
        }
    }

    #[cfg(unix)]
    mod fake_cli {
        use super::*;
        use llmux_types::{Billing, DeploymentIdx, QuotaSpec, RequestId};
        use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, time::Duration};

        fn dep(credential: &str, upstream_model: &str) -> Deployment {
            Deployment {
                headers: HeaderMap::new(),
                fallback: false,
                idx: DeploymentIdx(0),
                id: "agent".into(),
                provider: ProviderKind(provider::ACP_AGENT.into()),
                credential: credential.into(),
                base_url: provider::ACP_AGENT_BASE_URL.parse().unwrap(),
                upstream_model: upstream_model.into(),
                strip_request_fields: Box::default(),
                priority: 0,
                quota: QuotaSpec::default(),
                timeout: Duration::from_secs(30),
                max_context: None,
                expires_at: None,
                billing: Billing::Free,
            }
        }

        fn request(body: Value) -> GatewayRequest {
            GatewayRequest {
                format: InboundFormat::OpenAiChat,
                model: "agent".into(),
                stream: false,
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

        fn chat_body() -> Value {
            json!({"model":"agent","messages":[{"role":"user","content":"hi"}]})
        }

        fn spec(dir: &std::path::Path, mode: AcpSessionMode) -> AcpAgentSpec {
            AcpAgentSpec {
                command: dir.join("agent"),
                args: vec!["--acp".into()],
                env: BTreeMap::from([("AGENT_EXTRA".to_owned(), "on".to_owned())]),
                cwd: None,
                home: dir.to_owned(),
                turn_timeout: Duration::from_secs(10),
                mode,
            }
        }

        /// Writes a fake ACP CLI: a shell script whose env markers select the
        /// behaviors under test. The script answers the ACP JSON-RPC frames on
        /// stdin with canned `session/update` chunks from `chunks.txt`.
        fn script(dir: &std::path::Path, replies: &[&str]) -> AcpAgentSpec {
            std::fs::create_dir_all(dir.join(".local/share")).unwrap();
            let chunks: String = replies
                .iter()
                .map(|reply| {
                    json!({"jsonrpc":"2.0","method":"session/update",
                        "params":{"update":{"sessionUpdate":"agent_message_chunk",
                        "content":{"text":reply}}}})
                    .to_string()
                        + "\n"
                })
                .collect();
            std::fs::write(dir.join(".local/share/chunks.txt"), chunks).unwrap();
            let path = dir.join("agent");
            let body = r#"#!/bin/sh
# The sandbox is real: without the marker file the child exits non-zero.
test -f "$XDG_DATA_HOME/marker" || exit 3
if [ -f "$XDG_DATA_HOME/hang" ]; then sleep 60; fi
while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  printf '%s\n' "$line" >> "$XDG_DATA_HOME/requests.log"
  case "$line" in
    *'"initialize"'*)
      printf '%s\n' "$$" >> "$XDG_DATA_HOME/pids.log"
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id";;
    *'"session/new"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s1"}}\n' "$id";;
    *'"session/set_mode"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id";;
    *'"session/prompt"'*)
      printf '%s\n' "$line" >> "$XDG_DATA_HOME/prompts.log"
      if [ -f "$XDG_DATA_HOME/die" ]; then exit 7; fi
      if [ -f "$XDG_DATA_HOME/banner" ]; then printf 'AGENT-CLI banner v9\n'; fi
      if [ -f "$XDG_DATA_HOME/garbage" ]; then printf '{"broken json-rpc frame\n'; fi
      if [ -f "$XDG_DATA_HOME/fail_prompt" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"insufficient balance"}}\n' "$id"
        continue
      fi
      if [ -f "$XDG_DATA_HOME/ask_permission" ]; then
        printf '%s\n' '{"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{"sessionId":"s1","options":[{"optionId":"r1","kind":"reject_once"}]}}'
        read -r reply
        printf '%s\n' "$reply" >> "$XDG_DATA_HOME/perm_reply.log"
      fi
      cat "$XDG_DATA_HOME/chunks.txt"
      if [ -f "$XDG_DATA_HOME/with_usage" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn","usage":{"inputTokens":11,"outputTokens":7,"totalTokens":18,"cachedReadTokens":5}}}\n' "$id"
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      fi;;
  esac
done
"#
            .to_owned();
            // Rename into place: exec'ing a file another thread still holds
            // open for writing fails with ETXTBSY; a rename is atomic.
            let staged = dir.join("agent.tmp");
            std::fs::write(&staged, body).unwrap();
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&staged, &path).unwrap();
            std::fs::write(dir.join(".local/share/marker"), "").unwrap();
            spec(dir, AcpSessionMode::PerRequest)
        }

        fn pid_log(dir: &std::path::Path) -> Vec<String> {
            std::fs::read_to_string(dir.join(".local/share/pids.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        async fn complete(
            adapter: &AcpAgent,
            dep_: &Deployment,
            body: Value,
        ) -> Result<Value, UpstreamError> {
            let GatewayResponse::Complete {
                body: out, status, ..
            } = adapter.serve(&request(body), dep_).await?
            else {
                panic!("expected a complete response");
            };
            assert_eq!(status, StatusCode::OK);
            Ok(serde_json::from_slice(&out).unwrap())
        }

        async fn serve_err(adapter: &AcpAgent, dep_: &Deployment, body: Value) -> UpstreamError {
            match adapter.serve(&request(body), dep_).await {
                Err(err) => err,
                Ok(_) => panic!("expected an error"),
            }
        }

        #[tokio::test]
        async fn serve_runs_the_acp_conversation_and_maps_the_reply() {
            let dir = tempfile::tempdir().unwrap();
            let adapter = AcpAgent::new(HashMap::from([(
                "cli".into(),
                script(dir.path(), &["Hello ", "world"]),
            )]));
            let out = complete(&adapter, &dep("cli", "agent-1"), chat_body())
                .await
                .unwrap();
            assert_eq!(out["object"], "chat.completion");
            assert_eq!(out["model"], "agent-1");
            let choice = &out["choices"][0];
            assert_eq!(choice["finish_reason"], "stop");
            assert_eq!(choice["message"]["role"], "assistant");
            assert_eq!(choice["message"]["content"], "Hello world");
            let log =
                std::fs::read_to_string(dir.path().join(".local/share/requests.log")).unwrap();
            let order = |m: &str| log.find(m).unwrap_or_else(|| panic!("{m} in {log}"));
            assert!(
                order("initialize") < order("session/new")
                    && order("session/new") < order("session/set_mode")
                    && order("session/set_mode") < order("session/prompt"),
                "{log}"
            );
            let prompts =
                std::fs::read_to_string(dir.path().join(".local/share/prompts.log")).unwrap();
            assert!(prompts.contains("[Conversation]"), "{prompts}");
            assert!(prompts.contains("user: hi"), "{prompts}");
            // The deployment's upstream_model rides on session/new.
            assert!(log.contains(r#""model":"agent-1""#), "{log}");
        }

        #[tokio::test]
        async fn per_request_spawns_a_fresh_child_and_kills_it() {
            let dir = tempfile::tempdir().unwrap();
            let adapter = AcpAgent::new(HashMap::from([(
                "cli".into(),
                script(dir.path(), &["one"]),
            )]));
            for _ in 0..2 {
                complete(&adapter, &dep("cli", "m"), chat_body())
                    .await
                    .unwrap();
            }
            let pids = pid_log(dir.path());
            assert_eq!(pids.len(), 2, "one child per request: {pids:?}");
            // The child is dead after the turn: `kill -0` must fail for both.
            for pid in pids {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    let alive = std::process::Command::new("kill")
                        .args(["-0", &pid])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if !alive {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "pid {pid} still alive"
                    );
                    // The runtime must stay free to reap the zombie child —
                    // a blocking sleep would stall the process driver and
                    // `kill -0` would keep succeeding on the defunct pid.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }

        #[tokio::test]
        async fn spawn_failure_is_a_connect_error() {
            let dir = tempfile::tempdir().unwrap();
            let mut spec = spec(dir.path(), AcpSessionMode::PerRequest);
            spec.command = dir.path().join("missing-cli");
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let err = serve_err(&adapter, &dep("cli", "m"), chat_body()).await;
            assert!(matches!(err, UpstreamError::Connect), "{err}");
        }

        #[tokio::test]
        async fn an_unknown_credential_is_a_server_error() {
            let dir = tempfile::tempdir().unwrap();
            let adapter =
                AcpAgent::new(HashMap::from([("cli".into(), script(dir.path(), &["x"]))]));
            let err = serve_err(&adapter, &dep("other", "m"), chat_body()).await;
            assert!(matches!(err, UpstreamError::Server { .. }), "{err}");
        }

        #[tokio::test]
        async fn a_child_exiting_nonzero_is_a_stream_error_not_a_hang() {
            let dir = tempfile::tempdir().unwrap();
            let mut spec = script(dir.path(), &["x"]);
            std::fs::remove_file(dir.path().join(".local/share/marker")).unwrap();
            spec.turn_timeout = Duration::from_secs(5);
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let err = serve_err(&adapter, &dep("cli", "m"), chat_body()).await;
            assert!(
                matches!(
                    err,
                    UpstreamError::StreamBroken { .. } | UpstreamError::Connect
                ),
                "{err}"
            );
        }

        #[tokio::test]
        async fn a_child_dying_mid_turn_is_a_stream_error() {
            let dir = tempfile::tempdir().unwrap();
            let spec = script(dir.path(), &["x"]);
            std::fs::write(dir.path().join(".local/share/die"), "").unwrap();
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let err = serve_err(&adapter, &dep("cli", "m"), chat_body()).await;
            assert!(matches!(err, UpstreamError::StreamBroken { .. }), "{err}");
        }

        #[tokio::test]
        async fn a_turn_timeout_maps_to_a_timeout_error() {
            let dir = tempfile::tempdir().unwrap();
            let mut spec = script(dir.path(), &["x"]);
            spec.turn_timeout = Duration::from_millis(300);
            std::fs::write(dir.path().join(".local/share/hang"), "").unwrap();
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let started = std::time::Instant::now();
            let err = serve_err(&adapter, &dep("cli", "m"), chat_body()).await;
            assert!(matches!(err, UpstreamError::Timeout), "{err}");
            assert!(started.elapsed() < Duration::from_secs(5));
        }

        #[tokio::test]
        async fn a_malformed_jsonrpc_line_is_an_error_not_a_hang() {
            let dir = tempfile::tempdir().unwrap();
            let spec = script(dir.path(), &["x"]);
            std::fs::write(dir.path().join(".local/share/garbage"), "").unwrap();
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let err = serve_err(&adapter, &dep("cli", "m"), chat_body()).await;
            assert!(matches!(err, UpstreamError::Server { .. }), "{err}");
        }

        #[tokio::test]
        async fn non_json_noise_lines_are_skipped() {
            let dir = tempfile::tempdir().unwrap();
            let spec = script(dir.path(), &["ok"]);
            std::fs::write(dir.path().join(".local/share/banner"), "").unwrap();
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let out = complete(&adapter, &dep("cli", "m"), chat_body())
                .await
                .unwrap();
            assert_eq!(out["choices"][0]["message"]["content"], "ok");
        }

        #[tokio::test]
        async fn an_agent_rpc_error_maps_to_the_failover_class() {
            let dir = tempfile::tempdir().unwrap();
            let spec = script(dir.path(), &["x"]);
            std::fs::write(dir.path().join(".local/share/fail_prompt"), "").unwrap();
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let err = serve_err(&adapter, &dep("cli", "m"), chat_body()).await;
            assert!(matches!(err, UpstreamError::QuotaExhausted { .. }), "{err}");
        }

        #[tokio::test]
        async fn permission_requests_are_cancelled_not_errored() {
            let dir = tempfile::tempdir().unwrap();
            let spec = script(dir.path(), &["done"]);
            std::fs::write(dir.path().join(".local/share/ask_permission"), "").unwrap();
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let out = complete(&adapter, &dep("cli", "m"), chat_body())
                .await
                .unwrap();
            assert_eq!(out["choices"][0]["message"]["content"], "done");
            let log =
                std::fs::read_to_string(dir.path().join(".local/share/perm_reply.log")).unwrap();
            assert!(log.contains(r#""result":{"outcome":"cancelled"}"#), "{log}");
            assert!(!log.contains("-32601"), "{log}");
        }

        #[tokio::test]
        async fn acp_usage_is_reported_on_the_completion() {
            let dir = tempfile::tempdir().unwrap();
            let spec = script(dir.path(), &["ok"]);
            std::fs::write(dir.path().join(".local/share/with_usage"), "").unwrap();
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let out = complete(&adapter, &dep("cli", "m"), chat_body())
                .await
                .unwrap();
            assert_eq!(out["usage"]["prompt_tokens"], 11);
            assert_eq!(out["usage"]["completion_tokens"], 7);
            assert_eq!(out["usage"]["total_tokens"], 18);
            assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 5);
        }

        #[tokio::test]
        async fn a_persistent_child_is_reused_across_requests() {
            let dir = tempfile::tempdir().unwrap();
            let mut spec = script(dir.path(), &["one", "two"]);
            spec.mode = AcpSessionMode::Persistent;
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            for expected in ["onetwo", "onetwo"] {
                let out = complete(&adapter, &dep("cli", "m"), chat_body())
                    .await
                    .unwrap();
                assert_eq!(out["choices"][0]["message"]["content"], expected);
            }
            assert_eq!(pid_log(dir.path()).len(), 1, "one initialized child");
            let log =
                std::fs::read_to_string(dir.path().join(".local/share/requests.log")).unwrap();
            assert_eq!(log.matches("session/new").count(), 2, "{log}");
        }

        #[tokio::test]
        async fn a_dead_persistent_child_is_respawned() {
            let dir = tempfile::tempdir().unwrap();
            let mut spec = script(dir.path(), &["one"]);
            spec.mode = AcpSessionMode::Persistent;
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            complete(&adapter, &dep("cli", "m"), chat_body())
                .await
                .unwrap();
            // Kill the live child; the next request must respawn and succeed.
            std::fs::write(dir.path().join(".local/share/die"), "").unwrap();
            let err = serve_err(&adapter, &dep("cli", "m"), chat_body()).await;
            assert!(matches!(err, UpstreamError::StreamBroken { .. }), "{err}");
            std::fs::remove_file(dir.path().join(".local/share/die")).unwrap();
            let out = complete(&adapter, &dep("cli", "m"), chat_body())
                .await
                .unwrap();
            assert_eq!(out["choices"][0]["message"]["content"], "one");
            assert_eq!(pid_log(dir.path()).len(), 2, "respawned once");
        }

        #[tokio::test]
        async fn an_rpc_error_keeps_the_persistent_child() {
            let dir = tempfile::tempdir().unwrap();
            let mut spec = script(dir.path(), &["ok"]);
            spec.mode = AcpSessionMode::Persistent;
            std::fs::write(dir.path().join(".local/share/fail_prompt"), "").unwrap();
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let err = serve_err(&adapter, &dep("cli", "m"), chat_body()).await;
            assert!(matches!(err, UpstreamError::QuotaExhausted { .. }), "{err}");
            std::fs::remove_file(dir.path().join(".local/share/fail_prompt")).unwrap();
            let out = complete(&adapter, &dep("cli", "m"), chat_body())
                .await
                .unwrap();
            assert_eq!(out["choices"][0]["message"]["content"], "ok");
            assert_eq!(pid_log(dir.path()).len(), 1, "the same child answered");
        }

        #[tokio::test]
        async fn requests_without_messages_are_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let adapter =
                AcpAgent::new(HashMap::from([("cli".into(), script(dir.path(), &["x"]))]));
            let err = serve_err(&adapter, &dep("cli", "m"), json!({"model":"agent"})).await;
            assert!(matches!(err, UpstreamError::BadRequest { .. }), "{err}");
        }

        async fn served_stream(
            mut body: Value,
            dir: &std::path::Path,
            replies: &[&str],
        ) -> Vec<String> {
            use futures_util::StreamExt;
            let spec = script(dir, replies);
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let mut req = request(std::mem::take(&mut body));
            req.stream = true;
            let GatewayResponse::Stream(events) =
                adapter.serve(&req, &dep("cli", "m")).await.unwrap()
            else {
                panic!("expected a stream");
            };
            events
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|e| e.unwrap().data)
                .collect()
        }

        #[tokio::test]
        async fn stream_emits_one_delta_per_chunk() {
            let dir = tempfile::tempdir().unwrap();
            let data = served_stream(chat_body(), dir.path(), &["Hel", "lo ", "world"]).await;
            assert_eq!(*data.last().unwrap(), "[DONE]");
            let first: Value = serde_json::from_str(&data[0]).unwrap();
            assert_eq!(
                first["choices"][0]["delta"],
                json!({"role":"assistant","content":""})
            );
            let text: String = data
                .iter()
                .filter_map(|d| serde_json::from_str::<Value>(d).ok())
                .filter_map(|v| {
                    v["choices"][0]["delta"]["content"]
                        .as_str()
                        .map(str::to_owned)
                })
                .collect();
            assert_eq!(text, "Hello world");
            let finish = data
                .iter()
                .map(|d| serde_json::from_str::<Value>(d).unwrap_or_default())
                .any(|v| v["choices"][0]["finish_reason"] == "stop");
            assert!(finish, "{data:?}");
        }

        #[tokio::test]
        async fn a_persistent_stream_closes_after_the_turn() {
            let dir = tempfile::tempdir().unwrap();
            use futures_util::StreamExt;
            let mut spec = script(dir.path(), &["hi"]);
            spec.mode = AcpSessionMode::Persistent;
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let mut req = request(chat_body());
            req.stream = true;
            let GatewayResponse::Stream(events) =
                adapter.serve(&req, &dep("cli", "m")).await.unwrap()
            else {
                panic!("expected a stream");
            };
            // A session cached with a live chunk sender holds the receiver
            // open forever: the stream must still end once the turn does.
            let data = timeout(Duration::from_secs(10), events.collect::<Vec<_>>())
                .await
                .expect("a cached session must not hold the stream open")
                .into_iter()
                .map(|e| e.unwrap().data)
                .collect::<Vec<_>>();
            assert_eq!(data.last().unwrap(), "[DONE]");
        }

        #[tokio::test]
        async fn images_are_staged_as_files_and_indexed_in_the_prompt() {
            let dir = tempfile::tempdir().unwrap();
            let spec = script(dir.path(), &["ok"]);
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let body = json!({"model":"agent","messages":[{"role":"user","content":[
                {"type":"text","text":"look"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="}}
            ]}]});
            complete(&adapter, &dep("cli", "m"), body).await.unwrap();
            let log = std::fs::read_to_string(dir.path().join(".local/share/prompts.log")).unwrap();
            assert!(log.contains("[Images]"), "{log}");
            assert_eq!(log.matches(".llmux-images/").count(), 1, "{log}");
            // The staging dir is removed once the turn completes.
            let leftover = std::fs::read_dir(dir.path().join(".llmux-images"))
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);
            assert!(!leftover, "staged image dir must be cleaned up");
        }

        #[tokio::test]
        async fn stream_usage_is_emitted_only_when_requested() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join(".local/share")).unwrap();
            std::fs::write(dir.path().join(".local/share/with_usage"), "").unwrap();
            let mut body = chat_body();
            body["stream_options"] = json!({"include_usage":true});
            let data = served_stream(body, dir.path(), &["ok"]).await;
            let usage = data
                .iter()
                .map(|d| serde_json::from_str::<Value>(d).unwrap_or_default())
                .find(|v| v["choices"].as_array().is_some_and(|c| c.is_empty()))
                .expect("a usage chunk");
            assert_eq!(usage["usage"]["prompt_tokens"], 11);

            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join(".local/share")).unwrap();
            std::fs::write(dir.path().join(".local/share/with_usage"), "").unwrap();
            let data = served_stream(chat_body(), dir.path(), &["ok"]).await;
            assert!(!data.iter().any(|d| d.contains(r#""usage""#)), "{data:?}");
        }

        #[tokio::test]
        async fn stream_errors_surface_as_stream_items() {
            use futures_util::StreamExt;
            let dir = tempfile::tempdir().unwrap();
            let mut spec = spec(dir.path(), AcpSessionMode::PerRequest);
            spec.command = dir.path().join("missing-cli");
            let adapter = AcpAgent::new(HashMap::from([("cli".into(), spec)]));
            let mut req = request(chat_body());
            req.stream = true;
            let GatewayResponse::Stream(events) =
                adapter.serve(&req, &dep("cli", "m")).await.unwrap()
            else {
                panic!("expected a stream");
            };
            let items: Vec<_> = events.collect().await;
            assert_eq!(items.len(), 1);
            assert!(matches!(
                items[0].as_ref().unwrap_err(),
                UpstreamError::Connect
            ));
        }
    }
}
