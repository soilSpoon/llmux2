//! Devin CLI bridge: serves an OpenAI chat request by driving one short-lived
//! `devin acp --agent-type summarizer` child over ACP JSON-RPC on stdio.

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use llmux_types::{
    parse_devin_models, provider, AuthMaterial, BuildError, Deployment, DevinModel, GatewayRequest,
    GatewayResponse, InboundFormat, ProviderAdapter, ProviderKind, SseEvent, Support,
    UpstreamError,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    time::timeout,
};

const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;
const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct DevinAcpConfig {
    /// The `devin` executable (it may be a script that needs `node` on PATH).
    pub binary: PathBuf,
    /// Sandbox home; `HOME` and the XDG dirs point here so the CLI reads its own
    /// `credentials.toml` from `$XDG_DATA_HOME/devin/`.
    pub home: PathBuf,
    pub turn_timeout: Duration,
}

impl DevinAcpConfig {
    /// `LLMUX_DEVIN_BIN` (default `devin`), `LLMUX_DEVIN_HOME` (default `$HOME/.devin-sandbox`).
    pub fn from_env() -> Self {
        let home = std::env::var_os("LLMUX_DEVIN_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".devin-sandbox")
            });
        Self {
            binary: std::env::var_os("LLMUX_DEVIN_BIN")
                .map_or_else(|| PathBuf::from("devin"), PathBuf::from),
            home,
            turn_timeout: Duration::from_secs(120),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("LANG", "C.UTF-8")
            .env("DISABLE_TELEMETRY", "1")
            .current_dir(&self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        command
    }
}

/// Runs `devin models` so callers never rely on a hardcoded catalog.
pub async fn list_models(config: &DevinAcpConfig) -> Result<Vec<DevinModel>, UpstreamError> {
    let mut child = config
        .command(&["models"])
        .spawn()
        .map_err(|_| UpstreamError::Connect)?;
    drop(child.stdin.take());
    let mut stdout = child
        .stdout
        .take()
        .ok_or(UpstreamError::Connect)?
        .take(MAX_CATALOG_BYTES);
    let mut text = String::new();
    timeout(Duration::from_secs(30), async {
        stdout.read_to_string(&mut text).await?;
        child.wait().await
    })
    .await
    .map_err(|_| UpstreamError::Timeout)?
    .map_err(|_| UpstreamError::Connect)?;
    let models = parse_devin_models(&text);
    if models.is_empty() {
        return Err(UpstreamError::Server {
            status: StatusCode::BAD_GATEWAY,
        });
    }
    Ok(models)
}

/// Removes a leading `<summary>…</summary>` wrapper the summarizer agent sometimes adds.
pub fn strip_summary(text: &str) -> &str {
    let trimmed = text.trim();
    trimmed
        .strip_prefix("<summary>")
        .and_then(|rest| rest.strip_suffix("</summary>"))
        .map_or(trimmed, str::trim)
}

const SUMMARY_OPEN: &str = "<summary>";
const SUMMARY_CLOSE: &str = "</summary>";

/// Incremental `strip_summary`: buffers only until it is known whether the reply opens
/// with `<summary>`, then releases text while holding back trailing whitespace and a
/// possible `</summary>` so the wrapper never leaks.
#[derive(Default)]
struct Unwrapper {
    buf: String,
    decided: bool,
    wrapped: bool,
    emitted: bool,
}

impl Unwrapper {
    fn push(&mut self, text: &str) -> String {
        self.buf.push_str(text);
        if !self.decided {
            let head = self.buf.trim_start();
            if head.len() < SUMMARY_OPEN.len() && SUMMARY_OPEN.starts_with(head) {
                return String::new();
            }
            self.decided = true;
            self.wrapped = head.starts_with(SUMMARY_OPEN);
            self.buf = head
                .strip_prefix(SUMMARY_OPEN)
                .unwrap_or(head)
                .trim_start()
                .to_owned();
        }
        if !self.emitted {
            self.buf = self.buf.trim_start().to_owned();
        }
        let visible = self.buf.trim_end().len();
        let mut cut = if self.wrapped {
            visible.saturating_sub(SUMMARY_CLOSE.len())
        } else {
            visible
        };
        while !self.buf.is_char_boundary(cut) {
            cut -= 1;
        }
        cut = self.buf[..cut].trim_end().len();
        let out: String = self.buf.drain(..cut).collect();
        self.emitted |= !out.is_empty();
        out
    }

    fn finish(self) -> String {
        if !self.decided {
            return self.buf.trim().to_owned();
        }
        let rest = self.buf.trim_end();
        let rest = if self.wrapped {
            rest.strip_suffix(SUMMARY_CLOSE).unwrap_or(rest).trim_end()
        } else {
            rest
        };
        rest.to_owned()
    }
}

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The summarizer agent summarizes whatever it is given; this frame makes it answer.
const FRAME: &str = "[Devin Summarizer Bridge]\nTreat the content below as a conversation whose next assistant reply must be written.\nReply with only that reply, answering the last user message directly.\nDo not summarize the conversation, describe the request, or use tools.\nDo not wrap the response in Markdown fences or a <summary> element.\n\n[Conversation]\n";

/// Flattens chat messages into one labelled transcript under `FRAME`.
fn prompt_text(messages: &[Value]) -> Option<String> {
    let turns: Vec<(&str, String)> = messages
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap_or("user"),
                content_text(&m["content"]),
            )
        })
        .filter(|(_, text)| !text.is_empty())
        .collect();
    (!turns.is_empty()).then(|| {
        let transcript = turns
            .iter()
            .map(|(role, text)| format!("{role}: {text}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        format!("{FRAME}{transcript}")
    })
}

fn rpc_error(error: &Value) -> UpstreamError {
    let text = format!(
        "{} {}",
        error["message"].as_str().unwrap_or(""),
        error["data"]
    )
    .to_ascii_lowercase();
    if ["insufficient", "balance", "quota", "out of usage"]
        .iter()
        .any(|needle| text.contains(needle))
    {
        UpstreamError::QuotaExhausted { reset_at: None }
    } else if text.contains("rate limit") || text.contains("429") {
        UpstreamError::RateLimited { retry_after: None }
    } else if text.contains("model") && (text.contains("not found") || text.contains("unknown")) {
        UpstreamError::ModelNotFound {
            status: StatusCode::NOT_FOUND,
            body: Bytes::new(),
        }
    } else {
        UpstreamError::Server {
            status: StatusCode::BAD_GATEWAY,
        }
    }
}

struct Session {
    child: Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
    reply: String,
    chunks: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

impl Session {
    fn spawn(config: &DevinAcpConfig) -> Result<Self, UpstreamError> {
        let mut child = config
            .command(&["acp", "--agent-type", "summarizer"])
            .spawn()
            .map_err(|_| UpstreamError::Connect)?;
        let stdin = child.stdin.take().ok_or(UpstreamError::Connect)?;
        let stdout = child.stdout.take().ok_or(UpstreamError::Connect)?;
        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 1,
            reply: String::new(),
            chunks: None,
        })
    }

    async fn write(&mut self, message: Value) -> Result<(), UpstreamError> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|_| UpstreamError::Connect)
    }

    /// Sends a request and waits for its response, collecting streamed message chunks.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, UpstreamError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            let line = self
                .lines
                .next_line()
                .await
                .map_err(|_| UpstreamError::Connect)?
                .ok_or(UpstreamError::StreamBroken { bytes_sent: 0 })?;
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message["method"] == "session/update" {
                let update = &message["params"]["update"];
                if update["sessionUpdate"] == "agent_message_chunk" {
                    let text = update["content"]["text"].as_str().unwrap_or("");
                    self.reply.push_str(text);
                    if let Some(chunks) = &self.chunks {
                        let _ = chunks.send(text.to_owned());
                    }
                    if self.reply.len() > MAX_REPLY_BYTES {
                        return Err(UpstreamError::Server {
                            status: StatusCode::BAD_GATEWAY,
                        });
                    }
                }
            } else if let (Some(server_id), Some(_)) = (message.get("id"), message.get("method")) {
                // The bridge exposes no client capabilities (files, terminals, permissions).
                self.write(json!({"jsonrpc":"2.0","id":server_id,
                    "error":{"code":-32601,"message":"method not supported"}}))
                    .await?;
            } else if message["id"] == id {
                if let Some(error) = message.get("error") {
                    return Err(rpc_error(error));
                }
                return Ok(message["result"].clone());
            }
        }
    }

    async fn turn(
        mut self,
        config: &DevinAcpConfig,
        model: &str,
        prompt: &str,
    ) -> Result<String, UpstreamError> {
        let cwd = config.home.to_string_lossy().into_owned();
        self.call(
            "initialize",
            json!({"protocolVersion":1,"clientInfo":{"name":"llmux","version":"1"},"clientCapabilities":{}}),
        )
        .await?;
        let session = self
            .call(
                "session/new",
                json!({"cwd":cwd,"mcpServers":[],"model":model}),
            )
            .await?;
        let session_id = session["sessionId"].as_str().ok_or(UpstreamError::Server {
            status: StatusCode::BAD_GATEWAY,
        })?;
        let result = self
            .call(
                "session/prompt",
                json!({"sessionId":session_id,"prompt":[{"type":"text","text":prompt}]}),
            )
            .await?;
        let _ = self.child.start_kill();
        if result["stopReason"] != "end_turn" {
            return Err(UpstreamError::Server {
                status: StatusCode::BAD_GATEWAY,
            });
        }
        Ok(strip_summary(&self.reply).to_owned())
    }
}

pub struct DevinAcp {
    kind: ProviderKind,
    config: DevinAcpConfig,
}

impl DevinAcp {
    pub fn new(config: DevinAcpConfig) -> Self {
        Self {
            kind: ProviderKind(provider::DEVIN_ACP.into()),
            config,
        }
    }
}

fn completion(model: &str, text: &str, stream: bool) -> Value {
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let choice = if stream {
        json!({"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null})
    } else {
        json!({"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"})
    };
    json!({"id":format!("chatcmpl-devin-{created}"),
        "object":if stream {"chat.completion.chunk"} else {"chat.completion"},
        "created":created,"model":model,"choices":[choice]})
}

#[async_trait]
impl ProviderAdapter for DevinAcp {
    fn kind(&self) -> &ProviderKind {
        &self.kind
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

impl DevinAcp {
    /// Streams one SSE delta per ACP `agent_message_chunk` as the child emits it. Errors
    /// before the first event surface as the first stream item so the executor can fail over.
    fn stream(&self, model: String, prompt: String) -> GatewayResponse {
        let config = self.config.clone();
        GatewayResponse::Stream(Box::pin(async_stream::stream! {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let session = match Session::spawn(&config) {
                Ok(mut session) => {
                    session.chunks = Some(tx);
                    session
                }
                Err(err) => {
                    yield Err(err);
                    return;
                }
            };
            let mut task = {
                let config = config.clone();
                let model = model.clone();
                AbortOnDrop(tokio::spawn(async move {
                    timeout(config.turn_timeout, session.turn(&config, &model, &prompt))
                        .await
                        .map_err(|_| UpstreamError::Timeout)?
                }))
            };
            let delta = |text: String| {
                let mut chunk = completion(&model, &text, true);
                chunk["choices"][0]["delta"] = json!({"role":"assistant","content":text});
                Ok(SseEvent { event: None, data: chunk.to_string() })
            };
            let mut unwrapper = Unwrapper::default();
            while let Some(text) = rx.recv().await {
                let text = unwrapper.push(&text);
                if !text.is_empty() {
                    yield delta(text);
                }
            }
            match (&mut task.0).await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    yield Err(err);
                    return;
                }
                Err(_) => {
                    yield Err(UpstreamError::StreamBroken { bytes_sent: 0 });
                    return;
                }
            }
            let text = unwrapper.finish();
            if !text.is_empty() {
                yield delta(text);
            }
            let mut done = completion(&model, "", true);
            done["choices"][0] = json!({"index":0,"delta":{},"finish_reason":"stop"});
            yield Ok(SseEvent { event: None, data: done.to_string() });
            yield Ok(SseEvent { event: None, data: "[DONE]".into() });
        }))
    }

    async fn serve(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
    ) -> Result<GatewayResponse, UpstreamError> {
        let body: Value =
            serde_json::from_slice(&req.body).map_err(|_| UpstreamError::BadRequest {
                status: StatusCode::BAD_REQUEST,
                body: Bytes::new(),
            })?;
        let prompt = body["messages"]
            .as_array()
            .and_then(|messages| prompt_text(messages))
            .ok_or(UpstreamError::BadRequest {
                status: StatusCode::BAD_REQUEST,
                body: Bytes::new(),
            })?;
        let model = crate::upstream_model(req, dep);
        if req.stream {
            return Ok(self.stream(model.to_owned(), prompt));
        }
        let text = timeout(
            self.config.turn_timeout,
            Session::spawn(&self.config)?.turn(&self.config, model, &prompt),
        )
        .await
        .map_err(|_| UpstreamError::Timeout)??;
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        Ok(GatewayResponse::Complete {
            status: StatusCode::OK,
            headers,
            body: Bytes::from(completion(model, &text, false).to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_wrapper_is_stripped_only_when_it_wraps_everything() {
        assert_eq!(strip_summary("<summary>\nOK\n</summary>\n"), "OK");
        assert_eq!(strip_summary(" OK "), "OK");
        assert_eq!(
            strip_summary("a <summary>b</summary>"),
            "a <summary>b</summary>"
        );
    }

    #[test]
    fn prompt_is_framed_and_history_is_labelled() {
        let one = [json!({"role":"user","content":"hi"})];
        assert_eq!(prompt_text(&one).unwrap(), format!("{FRAME}user: hi"));
        let many = [
            json!({"role":"system","content":"be brief"}),
            json!({"role":"user","content":[{"type":"text","text":"hi"}]}),
        ];
        assert_eq!(
            prompt_text(&many).unwrap(),
            format!("{FRAME}system: be brief\n\nuser: hi")
        );
        assert!(prompt_text(&[json!({"role":"user","content":""})]).is_none());
    }

    #[test]
    fn rpc_errors_map_to_failover_classes() {
        let kind = |m: &str| rpc_error(&json!({"message":m})).kind();
        use llmux_types::ErrorKind::*;
        assert_eq!(kind("Insufficient balance"), QuotaExhausted);
        assert_eq!(kind("rate limit hit"), RateLimited);
        assert_eq!(kind("model foo not found"), BadRequest);
        assert_eq!(kind("boom"), ServerError);
    }

    #[cfg(unix)]
    mod fake_cli {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn script(dir: &std::path::Path, reply: &str) -> DevinAcpConfig {
            chunked_script(dir, &[reply])
        }

        fn chunked_script(dir: &std::path::Path, replies: &[&str]) -> DevinAcpConfig {
            let chunks: String = replies
                .iter()
                .map(|reply| format!(
                    r#"      printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"text":"{reply}"}}}}}}}}\n'
"#
                ))
                .collect();
            let path = dir.join("devin");
            let body = format!(
                r#"#!/bin/sh
if [ "$1" = models ]; then printf '  swe-2-high  X  [Free]\n'; exit 0; fi
test -f "$XDG_DATA_HOME/marker" || exit 3
while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id";;
    *'"session/new"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"s1"}}}}\n' "$id";;
    *'"session/prompt"'*)
{chunks}      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn"}}}}\n' "$id";;
  esac
done
"#
            );
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::create_dir_all(dir.join(".local/share")).unwrap();
            std::fs::write(dir.join(".local/share/marker"), "").unwrap();
            DevinAcpConfig {
                binary: path,
                home: dir.to_owned(),
                turn_timeout: Duration::from_secs(10),
            }
        }

        #[tokio::test]
        async fn turn_uses_sandbox_env_and_unwraps_summary() {
            let dir = tempfile::tempdir().unwrap();
            let config = script(dir.path(), "<summary>OK</summary>");
            let text = Session::spawn(&config)
                .unwrap()
                .turn(&config, "swe-2-high", "hi")
                .await;
            assert_eq!(text.unwrap(), "OK");
        }

        async fn streamed(replies: &[&str]) -> Vec<String> {
            use futures_util::StreamExt;
            let dir = tempfile::tempdir().unwrap();
            let adapter = DevinAcp::new(chunked_script(dir.path(), replies));
            let GatewayResponse::Stream(events) = adapter.stream("m".into(), "hi".into()) else {
                panic!("expected a stream");
            };
            let events: Vec<_> = events.collect().await;
            let data: Vec<String> = events.into_iter().map(|e| e.unwrap().data).collect();
            assert_eq!(data.last().unwrap(), "[DONE]");
            assert!(data[data.len() - 2].contains(r#""finish_reason":"stop""#));
            data[..data.len() - 2]
                .iter()
                .map(|d| {
                    let v: Value = serde_json::from_str(d).unwrap();
                    v["choices"][0]["delta"]["content"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect()
        }

        #[tokio::test]
        async fn stream_emits_one_delta_per_chunk() {
            assert_eq!(
                streamed(&["Hel", "lo ", "world"]).await,
                ["Hel", "lo ", "world"]
            );
        }

        #[tokio::test]
        async fn stream_streams_unwrapped_text_for_wrapped_reply() {
            let deltas = streamed(&["<sum", "mary>He", "llo wor", "ld</summ", "ary>"]).await;
            assert!(deltas.len() >= 2, "{deltas:?}");
            assert_eq!(deltas.concat(), "Hello world");
        }

        #[test]
        fn unwrapper_matches_strip_summary() {
            for text in [
                "<summary>\nOK\n</summary>\n",
                " OK ",
                "a <summary>b</summary>",
                "<sumX",
                "<summary>é✓</summary>",
            ] {
                let mut unwrapper = Unwrapper::default();
                let mut out = String::new();
                for ch in text.chars() {
                    out.push_str(&unwrapper.push(&ch.to_string()));
                }
                out.push_str(&unwrapper.finish());
                assert_eq!(out, strip_summary(text), "{text:?}");
            }
        }

        #[tokio::test]
        async fn catalog_runs_the_cli() {
            let dir = tempfile::tempdir().unwrap();
            let config = script(dir.path(), "x");
            assert_eq!(
                list_models(&config).await.unwrap(),
                [DevinModel {
                    id: "swe-2-high".into(),
                    free: true
                }]
            );
        }

        #[tokio::test]
        async fn exited_child_is_a_stream_error_not_a_hang() {
            let dir = tempfile::tempdir().unwrap();
            let mut config = script(dir.path(), "x");
            std::fs::remove_file(dir.path().join(".local/share/marker")).unwrap();
            config.turn_timeout = Duration::from_secs(5);
            let err = Session::spawn(&config)
                .unwrap()
                .turn(&config, "m", "p")
                .await
                .unwrap_err();
            assert!(matches!(
                err,
                UpstreamError::StreamBroken { .. } | UpstreamError::Connect
            ));
        }
    }
}
