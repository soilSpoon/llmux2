//! Devin CLI bridge: serves an OpenAI chat request by driving one short-lived
//! `devin acp` child (the default agent) over ACP JSON-RPC on stdio. The
//! `--agent-type summarizer` variant was dropped: given a long transcript it
//! fell back to emitting conversation summaries instead of a reply.

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use llmux_types::{
    header_request_id, parse_devin_models, provider, AuthMaterial, BuildError, Deployment,
    DevinModel, GatewayRequest, GatewayResponse, InboundFormat, ProviderAdapter, ProviderKind,
    SseEvent, Support, UpstreamError,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
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

/// Removes a leading `<summary>…</summary>` wrapper some Devin agents add.
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

/// Where a client image lives: bytes inline (any wire spelling) or a remote URL
/// the agent may fetch itself.
enum ImageSource {
    Data { mime: String, data: String },
    Url(String),
}

/// Detects the image part spellings the bridge accepts: OpenAI `image_url` /
/// `input_image` (string or `{url}` object) and Anthropic `image` sources.
fn image_source(part: &Value) -> Option<ImageSource> {
    let url_field = |key: &str| part[key]["url"].as_str().or_else(|| part[key].as_str());
    match part["type"].as_str() {
        Some("image_url") | Some("input_image") => url_field("image_url").and_then(url_source),
        Some("image") => match part["source"]["type"].as_str() {
            Some("base64") => Some(ImageSource::Data {
                mime: part["source"]["media_type"]
                    .as_str()
                    .unwrap_or("image/png")
                    .to_owned(),
                data: part["source"]["data"].as_str()?.to_owned(),
            }),
            Some("url") => part["source"]["url"].as_str().and_then(url_source),
            _ => None,
        },
        _ => None,
    }
}

/// A `data:` URL carries its bytes inline; anything else stays a URL for the
/// agent to resolve.
fn url_source(url: &str) -> Option<ImageSource> {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(',') {
            if let Some(mime) = meta.strip_suffix(";base64") {
                return Some(ImageSource::Data {
                    mime: mime.to_owned(),
                    data: data.to_owned(),
                });
            }
        }
    }
    (!url.is_empty()).then(|| ImageSource::Url(url.to_owned()))
}

/// Decoded image bytes must not exceed this: `devin acp` accepts inline
/// `image` blocks at the protocol level but never shows them to the model, so
/// images reach the model only as files the agent `read`s from its sandbox —
/// and unbounded writes are the failure to bound here.
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// Removes the staging directory when the guard drops — including aborted
/// turns, since the future's locals are dropped with it.
#[derive(Debug)]
struct ImageStaging {
    dir: PathBuf,
}

impl Drop for ImageStaging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Writes each inline image under `<home>/.llmux-images/<unique>/` and returns
/// the transcript index entries — file paths for inline payloads, the URL
/// itself for remote ones. The ACP `image` content block is intentionally not
/// used: the agent acknowledges it but the model never sees its bytes, while
/// a file it `read`s is rendered to the model (probed live on 3000.14.30821).
fn stage_images(
    home: &Path,
    messages: &[Value],
) -> Result<(Option<ImageStaging>, Vec<String>), UpstreamError> {
    use base64::Engine;
    let sources: Vec<ImageSource> = messages
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter_map(image_source)
        .collect();
    if sources.is_empty() {
        return Ok((None, Vec::new()));
    }
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = home
        .join(".llmux-images")
        .join(format!("{}-{unique}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|_| UpstreamError::Server {
        status: StatusCode::BAD_GATEWAY,
    })?;
    // Held from here so a mid-loop failure (bad base64, oversized payload)
    // removes the partially-written directory too.
    let staging = ImageStaging { dir };
    let mut entries = Vec::with_capacity(sources.len());
    for (index, source) in sources.into_iter().enumerate() {
        match source {
            ImageSource::Data { mime, data } => {
                // Base64 decodes to ~3/4 its length; reject by encoded length
                // first so an oversized payload is never materialized.
                if data.len() > MAX_IMAGE_BYTES * 4 / 3 + 4 {
                    return Err(UpstreamError::BadRequest {
                        status: StatusCode::PAYLOAD_TOO_LARGE,
                        body: Bytes::new(),
                    });
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&data)
                    .map_err(|_| UpstreamError::BadRequest {
                        status: StatusCode::BAD_REQUEST,
                        body: Bytes::new(),
                    })?;
                let ext = mime.strip_prefix("image/").unwrap_or("bin");
                let path = staging.dir.join(format!("image-{}.{}", index + 1, ext));
                std::fs::write(&path, bytes).map_err(|_| UpstreamError::Server {
                    status: StatusCode::BAD_GATEWAY,
                })?;
                entries.push(path.to_string_lossy().into_owned());
            }
            ImageSource::Url(url) => entries.push(url),
        }
    }
    Ok((Some(staging), entries))
}

/// The default agent continues whatever conversation it is given; this frame pins
/// the bridge contract so the reply is one assistant message and nothing else.
const FRAME: &str = "[Devin Chat Bridge]\nA conversation transcript follows under [Conversation]. Write the next assistant reply to it.\nReply with only that reply: plain text, no commentary about the conversation, no Markdown fences, and never use your own tools.\n";

/// With client tools offered, the bridge also accepts the structured-call reply:
/// the whole message is then one `{"tool_calls":[…]}` object emitted verbatim.
const TOOL_PROTOCOL: &str = "\nThe client offers the tools listed under [Tools]. If the next reply needs tool calls, reply with only one JSON object — {\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"<name>\",\"arguments\":\"<json-string>\"}}]} — and no other text. The client executes those calls; never run your own tools instead.\n\n[Tools]\n";

/// Devin's effort vocabulary: the swe-2 families publish one free model id per
/// tier (`swe-2-medium`, `swe-2-high`, `swe-2-max`), so every client spelling
/// lands on a free tier — `low`/`off` have no lighter free variant than medium.
fn effort_tier(body: &Value) -> Option<&'static str> {
    ["reasoning_effort", "thinking_effort", "effort"]
        .iter()
        .find_map(|field| body.get(*field).and_then(Value::as_str))
        .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disabled" | "false" | "low" | "minimal" | "medium" | "med" => {
                Some("medium")
            }
            "high" | "enabled" | "true" => Some("high"),
            "max" | "xhigh" => Some("max"),
            _ => None,
        })
}

/// `swe-2-high` + effort `max` -> `swe-2-max`; a `-lightning` suffix rides along.
/// A model id without a tier suffix keeps the deployment's spelling: guessing one
/// could mint ids the family never published (swe-1-7 has no `-high`, for example).
fn model_for(upstream_model: &str, effort: Option<&str>) -> String {
    let Some(tier) = effort else {
        return upstream_model.to_owned();
    };
    let (base, lightning) = upstream_model
        .strip_suffix("-lightning")
        .map_or((upstream_model, ""), |stem| (stem, "-lightning"));
    for published in ["medium", "high", "max"] {
        if let Some(stem) = base.strip_suffix(&format!("-{published}")) {
            return format!("{stem}-{tier}{lightning}");
        }
    }
    upstream_model.to_owned()
}

/// Flattens chat messages into one labelled transcript under `FRAME`, adding the
/// tool-call protocol and schemas when `tools` holds a non-empty array. An
/// assistant turn that only made calls still shows up — as `[tool_calls: …]` —
/// so the agent sees the loop it is inside; `tool` results keep their call id
/// (and name when present) so the model can pair call and result.
fn prompt_text(
    messages: &[Value],
    tools: Option<&Value>,
    tool_choice: &Value,
    parallel_tool_calls: bool,
) -> Option<String> {
    let mut image_index = 0usize;
    let turns: Vec<(String, String)> = messages
        .iter()
        .map(|m| {
            let role = m["role"].as_str().unwrap_or("user");
            let mut text = match &m["content"] {
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|part| {
                        if image_source(part).is_some() {
                            image_index += 1;
                            return Some(format!("[image {image_index}]"));
                        }
                        part["text"].as_str().map(str::to_owned)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                other => content_text(other),
            };
            if role == "assistant" {
                if let Some(calls) = m["tool_calls"].as_array().filter(|c| !c.is_empty()) {
                    let calls = format!("[tool_calls: {}]", Value::Array(calls.clone()));
                    text = if text.is_empty() {
                        calls
                    } else {
                        format!("{text}\n{calls}")
                    };
                }
            }
            let label = match role {
                "tool" => {
                    let id = m["tool_call_id"].as_str().unwrap_or_default();
                    let name = m["name"].as_str().unwrap_or_default();
                    match (id.is_empty(), name.is_empty()) {
                        (false, false) => format!("tool({id}, {name})"),
                        (false, true) => format!("tool({id})"),
                        (true, false) => format!("tool({name})"),
                        _ => "tool".to_owned(),
                    }
                }
                _ => role.to_owned(),
            };
            (label, text)
        })
        .filter(|(_, text)| !text.is_empty())
        .collect();
    (!turns.is_empty()).then(|| {
        let mut prompt = String::from(FRAME);
        if let Some(tools) = tools.and_then(Value::as_array).filter(|t| !t.is_empty()) {
            prompt.push_str(TOOL_PROTOCOL);
            prompt.push_str(&Value::Array(tools.clone()).to_string());
            if !parallel_tool_calls {
                prompt.push_str("\n\nAnswer with at most one tool call.");
            }
            if tool_choice.as_str() == Some("required") {
                prompt.push_str("\n\nYou must call at least one tool.");
            } else if tool_choice["type"].as_str() == Some("function") {
                if let Some(name) = tool_choice["function"]["name"].as_str() {
                    prompt.push_str(&format!("\n\nYou must call the tool {name}."));
                }
            }
        }
        let transcript = turns
            .iter()
            .map(|(role, text)| format!("{role}: {text}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        prompt.push_str("\n\n[Conversation]\n");
        prompt.push_str(&transcript);
        prompt
    })
}

/// What a collected reply means on the wire.
enum Reply {
    Text(String),
    /// The `tool_calls` array, normalized for the OpenAI shape.
    ToolCalls(Vec<Value>),
}

/// `tools_offered` gates the JSON probe so a plain answer that merely resembles
/// JSON is never promoted to calls.
fn classify(reply: &str, tools_offered: bool) -> Reply {
    let text = strip_summary(reply).trim().to_owned();
    if tools_offered {
        if let Some(calls) = tool_calls_in(&text) {
            return Reply::ToolCalls(calls);
        }
    }
    Reply::Text(text)
}

/// Pulls a `{"tool_calls":[…]}` array out of the reply: the whole text first,
/// then a `{"tool_calls"` object embedded in Markdown fences or lead-in prose,
/// which chat models occasionally wrap the payload in.
fn tool_calls_in(text: &str) -> Option<Vec<Value>> {
    fn calls_of(v: &Value) -> Option<Vec<Value>> {
        v["tool_calls"]
            .as_array()
            .filter(|a| !a.is_empty())
            .cloned()
    }
    if let Some(calls) = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| calls_of(&v))
    {
        return Some(normalize_calls(calls));
    }
    let start = text.find("{\"tool_calls\"")?;
    serde_json::Deserializer::from_str(&text[start..])
        .into_iter::<Value>()
        .next()?
        .ok()
        .and_then(|v| calls_of(&v))
        .map(normalize_calls)
}

/// Fills the fields the OpenAI shape requires but the model may drop: `id`,
/// `type`, the `function` wrapper for flat `{"name","arguments"}` calls, and
/// `arguments` as a JSON string.
fn normalize_calls(calls: Vec<Value>) -> Vec<Value> {
    calls
        .into_iter()
        .enumerate()
        .map(|(i, mut call)| {
            if call["id"].as_str().is_none_or(str::is_empty) {
                call["id"] = json!(format!("call_{i}"));
            }
            if call["type"].as_str().is_none_or(str::is_empty) {
                call["type"] = json!("function");
            }
            if !call["function"].is_object() && call["name"].is_string() {
                call["function"] = json!({
                    "name": call["name"].take(),
                    "arguments": call["arguments"].take(),
                });
            }
            let args = &call["function"]["arguments"];
            if args.is_null() {
                call["function"]["arguments"] = json!("{}");
            } else if !args.is_string() {
                call["function"]["arguments"] = json!(args.to_string());
            }
            call
        })
        .collect()
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
    /// Flipped on the first `session/update`, which is when the stream can tell
    /// clients the turn is alive (the marker is an empty string the consumer
    /// filters out — real chunks follow).
    announced: bool,
    chunks: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

impl Session {
    /// Spawns the CLI, retrying while the kernel reports the binary as busy.
    ///
    /// Replacing a running binary in place leaves `ETXTBSY` on the next exec until the
    /// last process holding it exits, and a freshly written script can be held the same
    /// way by a child another thread forked mid-write. Neither is a real failure.
    fn spawn_binary(config: &DevinAcpConfig) -> Result<tokio::process::Child, UpstreamError> {
        let mut attempt = 0;
        loop {
            match config.command(&["acp"]).spawn() {
                Ok(child) => return Ok(child),
                Err(err)
                    if err.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 20 =>
                {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return Err(UpstreamError::Connect),
            }
        }
    }

    fn spawn(config: &DevinAcpConfig) -> Result<Self, UpstreamError> {
        let mut child = Self::spawn_binary(config)?;
        let stdin = child.stdin.take().ok_or(UpstreamError::Connect)?;
        let stdout = child.stdout.take().ok_or(UpstreamError::Connect)?;
        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 1,
            reply: String::new(),
            announced: false,
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

    /// Writes a request without waiting for its response. Used for calls whose
    /// answer is best-effort: the reply, if one arrives, lands on the shared
    /// read loop inside `call` and is skipped by the id match.
    async fn send(&mut self, method: &str, params: Value) -> Result<(), UpstreamError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await
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
                if !self.announced {
                    self.announced = true;
                    if let Some(chunks) = &self.chunks {
                        let _ = chunks.send(String::new());
                    }
                }
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
            } else if let (Some(server_id), Some(method)) =
                (message.get("id"), message.get("method"))
            {
                // Sessions run in `ask` mode, so no tool call should arrive; any
                // that does is refused with a real ACP outcome — an error reply
                // would be the agent's signal to stop the turn empty-handed.
                let reply = if method.as_str() == Some("session/request_permission") {
                    json!({"jsonrpc":"2.0","id":server_id,
                        "result":{"outcome":{"outcome":"cancelled"}}})
                } else {
                    json!({"jsonrpc":"2.0","id":server_id,
                        "error":{"code":-32601,"message":"method not supported"}})
                };
                self.write(reply).await?;
            } else if message["id"] == id {
                if let Some(error) = message.get("error") {
                    return Err(rpc_error(error));
                }
                return Ok(message["result"].clone());
            }
        }
    }

    /// Returns the collected reply text plus the turn usage when the agent
    /// reports it (both are needed for an OpenAI-shaped answer). `prompt` is the
    /// ACP content-block array (the transcript text block); `_staged` keeps any
    /// image files alive until the turn — or an aborted one — ends.
    async fn turn(
        mut self,
        config: &DevinAcpConfig,
        model: &str,
        prompt: &[Value],
        _staged: Option<ImageStaging>,
    ) -> Result<(String, Option<Value>), UpstreamError> {
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
        // `ask` ("answer questions without code changes") turns the internal
        // tools off agent-side — a structural bound, unlike the prompt wording.
        // Sent without awaiting: CLIs that predate the method never reply, and
        // a reply that does arrive is ignored by the prompt call's id match.
        let _ = self
            .send(
                "session/set_mode",
                json!({"sessionId":session_id,"modeId":"ask"}),
            )
            .await;
        let result = self
            .call(
                "session/prompt",
                json!({"sessionId":session_id,"prompt":prompt}),
            )
            .await?;
        let _ = self.child.start_kill();
        if result["stopReason"] != "end_turn" {
            return Err(UpstreamError::Server {
                status: StatusCode::BAD_GATEWAY,
            });
        }
        Ok((
            strip_summary(&self.reply).to_owned(),
            result.get("usage").filter(|u| u.is_object()).cloned(),
        ))
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

/// Maps the ACP `session/prompt` usage block onto the OpenAI usage shape —
/// `prompt_tokens_details.cached_tokens` is the one detail field clients read.
fn usage_body(usage: Option<Value>) -> Option<Value> {
    let usage = usage?;
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let mut out = json!({
        "prompt_tokens": count("inputTokens"),
        "completion_tokens": count("outputTokens"),
        "total_tokens": count("totalTokens"),
    });
    if let Some(cached) = usage["cachedReadTokens"].as_u64() {
        out["prompt_tokens_details"] = json!({"cached_tokens": cached});
    }
    Some(out)
}

/// One shot carrying the whole `tool_calls` array — the JSON must be complete
/// before it can be re-emitted, so tool replies never stream incrementally.
fn tool_completion(model: &str, calls: &[Value], stream: bool) -> Value {
    let mut out = completion(model, "", stream);
    let calls: Vec<Value> = if stream {
        calls
            .iter()
            .enumerate()
            .map(|(i, call)| {
                let mut call = call.clone();
                call["index"] = json!(i);
                call
            })
            .collect()
    } else {
        calls.to_vec()
    };
    out["choices"][0] = if stream {
        // The role-only delta already announced the turn, so this delta keeps
        // only what changes: the calls themselves.
        json!({"index":0,"delta":{"content":null,
            "tool_calls":calls},"finish_reason":null})
    } else {
        json!({"index":0,"message":{"role":"assistant","content":null,
            "tool_calls":calls},"finish_reason":"tool_calls"})
    };
    out
}

#[async_trait]
impl ProviderAdapter for DevinAcp {
    fn kind(&self) -> &ProviderKind {
        &self.kind
    }

    /// Complete bodies are synthesized locally (`chatcmpl-devin-{secs}` ids are
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

impl DevinAcp {
    /// Streams one SSE delta per ACP `agent_message_chunk` as the child emits it — except
    /// when tools were offered: the reply must then be complete before it can be classified
    /// as text or `tool_calls`, so those turns buffer and emit in a single shot. The first
    /// `session/update` announces the turn with the OpenAI role-only delta, so a buffered
    /// turn still answers early instead of sitting silent until the reply completes.
    /// Errors before the first event surface as the first stream item so the executor can
    /// fail over.
    fn stream(
        &self,
        model: String,
        prompt: Vec<Value>,
        staged: Option<ImageStaging>,
        tools_offered: bool,
        include_usage: bool,
    ) -> GatewayResponse {
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
                    timeout(
                        config.turn_timeout,
                        session.turn(&config, &model, &prompt, staged),
                    )
                    .await
                    .map_err(|_| UpstreamError::Timeout)?
                }))
            };
            let delta = |text: String| {
                let mut chunk = completion(&model, &text, true);
                chunk["choices"][0]["delta"] = json!({"content":text});
                Ok(SseEvent { event: None, data: chunk.to_string() })
            };
            // The role-only delta mirrors the first chunk OpenAI servers send;
            // it goes out on the first marker/chunk, never ahead of an error.
            let mut announced = false;
            let announce = |announced: &mut bool| {
                if !*announced {
                    *announced = true;
                    let mut chunk = completion(&model, "", true);
                    chunk["choices"][0]["delta"] =
                        json!({"role":"assistant","content":""});
                    Some(SseEvent { event: None, data: chunk.to_string() })
                } else {
                    None
                }
            };
            let usage;
            if tools_offered {
                let mut reply = String::new();
                while let Some(text) = rx.recv().await {
                    if let Some(event) = announce(&mut announced) {
                        yield Ok(event);
                    }
                    reply.push_str(&text);
                }
                match (&mut task.0).await {
                    Ok(Ok((_, u))) => usage = u,
                    Ok(Err(err)) => {
                        yield Err(err);
                        return;
                    }
                    Err(_) => {
                        yield Err(UpstreamError::StreamBroken { bytes_sent: 0 });
                        return;
                    }
                }
                let finish = match classify(&reply, true) {
                    Reply::ToolCalls(calls) => {
                        let chunk = tool_completion(&model, &calls, true);
                        yield Ok(SseEvent { event: None, data: chunk.to_string() });
                        "tool_calls"
                    }
                    Reply::Text(text) => {
                        if !text.is_empty() {
                            yield delta(text);
                        }
                        "stop"
                    }
                };
                let mut done = completion(&model, "", true);
                done["choices"][0] = json!({"index":0,"delta":{},"finish_reason":finish});
                yield Ok(SseEvent { event: None, data: done.to_string() });
                if include_usage {
                    if let Some(usage) = usage_body(usage) {
                        let mut chunk = completion(&model, "", true);
                        chunk["choices"] = json!([]);
                        chunk["usage"] = usage;
                        yield Ok(SseEvent { event: None, data: chunk.to_string() });
                    }
                }
                yield Ok(SseEvent { event: None, data: "[DONE]".into() });
                return;
            }
            let mut unwrapper = Unwrapper::default();
            while let Some(text) = rx.recv().await {
                if let Some(event) = announce(&mut announced) {
                    yield Ok(event);
                }
                let text = unwrapper.push(&text);
                if !text.is_empty() {
                    yield delta(text);
                }
            }
            match (&mut task.0).await {
                Ok(Ok((_, u))) => usage = u,
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
            if include_usage {
                if let Some(usage) = usage_body(usage) {
                    let mut chunk = completion(&model, "", true);
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
        let body: Value =
            serde_json::from_slice(&req.body).map_err(|_| UpstreamError::BadRequest {
                status: StatusCode::BAD_REQUEST,
                body: Bytes::new(),
            })?;
        // `tool_choice: "none"` means the client wants a plain answer even while
        // advertising tools, so the protocol and schemas stay out of the prompt.
        let tools_offered = body["tools"].as_array().is_some_and(|t| !t.is_empty())
            && body["tool_choice"].as_str() != Some("none");
        let messages = body["messages"]
            .as_array()
            .ok_or(UpstreamError::BadRequest {
                status: StatusCode::BAD_REQUEST,
                body: Bytes::new(),
            })?;
        let prompt = prompt_text(
            messages,
            body.get("tools").filter(|_| tools_offered),
            &body["tool_choice"],
            body["parallel_tool_calls"].as_bool().unwrap_or(true),
        )
        .ok_or(UpstreamError::BadRequest {
            status: StatusCode::BAD_REQUEST,
            body: Bytes::new(),
        })?;
        let (staged, image_entries) = stage_images(&self.config.home, messages)?;
        let mut prompt = prompt;
        if !image_entries.is_empty() {
            prompt.push_str("\n\n[Images]");
            for (index, entry) in image_entries.iter().enumerate() {
                prompt.push_str(&format!("\nimage {}: {entry}", index + 1));
            }
        }
        let blocks = vec![json!({"type":"text","text":prompt})];
        let model = model_for(crate::upstream_model(req, dep), effort_tier(&body));
        if req.stream {
            let include_usage = body["stream_options"]["include_usage"].as_bool() == Some(true);
            return Ok(self.stream(model, blocks, staged, tools_offered, include_usage));
        }
        let (text, usage) = timeout(
            self.config.turn_timeout,
            Session::spawn(&self.config)?.turn(&self.config, &model, &blocks, staged),
        )
        .await
        .map_err(|_| UpstreamError::Timeout)??;
        let mut body = match classify(&text, tools_offered) {
            Reply::ToolCalls(calls) => tool_completion(&model, &calls, false),
            Reply::Text(text) => completion(&model, &text, false),
        };
        if let Some(usage) = usage_body(usage) {
            body["usage"] = usage;
        }
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        Ok(GatewayResponse::Complete {
            status: StatusCode::OK,
            headers,
            body: Bytes::from(body.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesized_completion_id_is_not_an_upstream_request_id() {
        let adapter = DevinAcp::new(DevinAcpConfig {
            binary: PathBuf::from("devin"),
            home: PathBuf::from("/nonexistent"),
            turn_timeout: Duration::from_secs(120),
        });
        let body = Bytes::from(completion("swe-2-high", "hi", false).to_string());
        assert!(adapter
            .upstream_request_id(&HeaderMap::new(), Some(&body))
            .is_none());
    }

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
        let none = &Value::Null;
        let one = [json!({"role":"user","content":"hi"})];
        assert_eq!(
            prompt_text(&one, None, none, true).unwrap(),
            format!("{FRAME}\n\n[Conversation]\nuser: hi")
        );
        let many = [
            json!({"role":"system","content":"be brief"}),
            json!({"role":"user","content":[{"type":"text","text":"hi"}]}),
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"c1","content":"a1-free-2"}),
        ];
        assert_eq!(
            prompt_text(&many, None, none, true).unwrap(),
            format!("{FRAME}\n\n[Conversation]\nsystem: be brief\n\nuser: hi\n\nassistant: [tool_calls: [{{\"function\":{{\"arguments\":\"{{}}\",\"name\":\"read\"}},\"id\":\"c1\",\"type\":\"function\"}}]]\n\ntool(c1): a1-free-2")
        );
        assert!(prompt_text(&[json!({"role":"user","content":""})], None, none, true).is_none());
    }

    #[test]
    fn tool_turns_keep_the_call_identity() {
        let none = &Value::Null;
        let msgs = [
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"call_1","name":"read","content":"a1-free-2"}),
            json!({"role":"tool","tool_call_id":"call_2","content":"ok"}),
        ];
        let prompt = prompt_text(&msgs, None, none, true).unwrap();
        assert!(prompt.contains("tool(call_1, read): a1-free-2"), "{prompt}");
        assert!(prompt.contains("tool(call_2): ok"), "{prompt}");
    }

    #[test]
    fn an_assistant_turn_keeps_its_text_and_its_calls() {
        let none = &Value::Null;
        let msgs = [
            json!({"role":"assistant","content":"let me check","tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]}),
        ];
        let prompt = prompt_text(&msgs, None, none, true).unwrap();
        assert!(
            prompt.contains("assistant: let me check\n[tool_calls:"),
            "{prompt}"
        );
    }

    #[test]
    fn a_named_tool_choice_forces_that_function() {
        let msgs = [json!({"role":"user","content":"hi"})];
        let tools = json!([{"type":"function","function":{"name":"read","parameters":{}}}]);
        let choice = json!({"type":"function","function":{"name":"read"}});
        let prompt = prompt_text(&msgs, Some(&tools), &choice, true).unwrap();
        assert!(prompt.contains("You must call the tool read."), "{prompt}");
        let auto = json!("required");
        let prompt = prompt_text(&msgs, Some(&tools), &auto, true).unwrap();
        assert!(
            prompt.contains("You must call at least one tool."),
            "{prompt}"
        );
    }

    #[test]
    fn parallel_tool_calls_false_caps_the_protocol() {
        let msgs = [json!({"role":"user","content":"hi"})];
        let tools = json!([{"type":"function","function":{"name":"read","parameters":{}}}]);
        let none = &Value::Null;
        let prompt = prompt_text(&msgs, Some(&tools), none, false).unwrap();
        assert!(prompt.contains("at most one tool"), "{prompt}");
        let prompt = prompt_text(&msgs, Some(&tools), none, true).unwrap();
        assert!(!prompt.contains("at most one tool"), "{prompt}");
    }

    #[test]
    fn image_parts_are_numbered_in_the_transcript() {
        let none = &Value::Null;
        let msgs = [
            json!({"role":"user","content":[
                {"type":"text","text":"first"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]}),
            json!({"role":"assistant","content":"a cat"}),
            json!({"role":"user","content":[
                {"type":"image_url","image_url":{"url":"data:image/jpeg;base64,BB=="}},
                {"type":"text","text":"second"}]}),
        ];
        let prompt = prompt_text(&msgs, None, none, true).unwrap();
        assert!(prompt.contains("user: first\n[image 1]"), "{prompt}");
        assert!(prompt.contains("user: [image 2]\nsecond"), "{prompt}");
        assert!(prompt.contains("assistant: a cat"), "{prompt}");
    }

    #[test]
    fn image_parts_are_staged_as_readable_files() {
        let home = tempfile::tempdir().unwrap();
        let msgs = [
            json!({"role":"user","content":[
                {"type":"text","text":"hi"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]}),
            json!({"role":"user","content":[
                {"type":"input_image","image_url":"data:image/webp;base64,WQ=="}]}),
            json!({"role":"user","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"Sko="}}]}),
            json!({"role":"user","content":[
                {"type":"image_url","image_url":{"url":"https://example.test/x.png"}}]}),
            json!({"role":"user","content":"plain"}),
        ];
        let (staged, entries) = stage_images(home.path(), &msgs).unwrap();
        let staged = staged.unwrap();
        assert_eq!(entries.len(), 4);
        for (i, (entry, ext, bytes)) in [
            (&entries[0], "png", vec![0x00]),
            (&entries[1], "webp", vec![0x59]),
            (&entries[2], "jpeg", vec![0x4a, 0x4a]),
        ]
        .iter()
        .enumerate()
        {
            let path = Path::new(entry.as_str());
            assert!(path.starts_with(&staged.dir), "{entry}");
            assert_eq!(path.extension().unwrap(), *ext, "{entry}");
            assert_eq!(std::fs::read(path).unwrap(), *bytes, "{entry}");
            let _ = i;
        }
        assert_eq!(entries[3], "https://example.test/x.png");
        let dir = staged.dir.clone();
        drop(staged);
        assert!(!dir.exists(), "staging dir must be removed on drop");
    }

    #[test]
    fn oversized_or_malformed_images_are_rejected() {
        let home = tempfile::tempdir().unwrap();
        let big = "AAAA".repeat(MAX_IMAGE_BYTES); // canonical, past the cap
        let msgs = [json!({"role":"user","content":[
            {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{big}")}}
        ]})];
        let err = stage_images(home.path(), &msgs).unwrap_err();
        assert!(matches!(
            err,
            UpstreamError::BadRequest {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                ..
            }
        ));
        assert_eq!(
            std::fs::read_dir(home.path().join(".llmux-images"))
                .map(|d| d.count())
                .unwrap_or(0),
            0,
            "failed staging must leave no files behind"
        );

        let msgs = [json!({"role":"user","content":[
            {"type":"image_url","image_url":{"url":"data:image/png;base64,!!!"}}
        ]})];
        assert!(matches!(
            stage_images(home.path(), &msgs),
            Err(UpstreamError::BadRequest {
                status: StatusCode::BAD_REQUEST,
                ..
            })
        ));
    }

    #[test]
    fn tool_calls_replies_are_classified_and_normalized() {
        let calls = |text: &str| match classify(text, true) {
            Reply::ToolCalls(calls) => calls,
            Reply::Text(text) => panic!("expected tool_calls, got text {text:?}"),
        };
        assert_eq!(
            calls(r#"{"tool_calls":[{"function":{"name":"read","arguments":{"p":1}}}]}"#)[0]
                ["function"]["arguments"],
            r#"{"p":1}"#
        );
        assert_eq!(calls("```json\n{\"tool_calls\":[{\"id\":\"x\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]}\n```")[0]["id"], "x");
        assert!(matches!(classify("{\"note\":1}", true), Reply::Text(_)));
        assert!(matches!(
            classify("{\"tool_calls\":[]}", true),
            Reply::Text(_)
        ));
        assert!(matches!(
            classify(
                r#"{"tool_calls":[{"function":{"name":"f","arguments":"{}"}}]}"#,
                false
            ),
            Reply::Text(_)
        ));
        // Chat models often answer with the flat {"name":…,"arguments":…}
        // spelling; the wire shape still needs id/type/function.
        let flat = calls(r#"{"tool_calls":[{"name":"f","arguments":{"p":1}}]}"#);
        assert_eq!(flat[0]["type"], "function");
        assert_eq!(flat[0]["function"]["name"], "f");
        assert_eq!(flat[0]["function"]["arguments"], r#"{"p":1}"#);
        let bare = calls(r#"{"tool_calls":[{"function":{"name":"f"}}]}"#);
        assert_eq!(bare[0]["function"]["arguments"], "{}");
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
        use llmux_types::{Billing, DeploymentIdx, QuotaSpec, RequestId};
        use std::os::unix::fs::PermissionsExt;

        fn dep(upstream_model: &str) -> Deployment {
            Deployment {
                headers: HeaderMap::new(),
                fallback: false,
                idx: DeploymentIdx(0),
                id: "devin".into(),
                provider: ProviderKind(provider::DEVIN_ACP.into()),
                credential: "none".into(),
                base_url: provider::DEVIN_ACP_BASE_URL.parse().unwrap(),
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
                model: "swe-2".into(),
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

        async fn complete_model(body: Value, upstream_model: &str) -> String {
            let dir = tempfile::tempdir().unwrap();
            let adapter = DevinAcp::new(script(dir.path(), "ok"));
            let GatewayResponse::Complete { body: out, .. } = adapter
                .serve(&request(body), &dep(upstream_model))
                .await
                .unwrap()
            else {
                panic!("expected a complete response");
            };
            let out: Value = serde_json::from_slice(&out).unwrap();
            out["model"].as_str().unwrap().to_owned()
        }

        fn chat_body(effort: Value) -> Value {
            json!({"model":"swe-2","reasoning_effort":effort,
                "messages":[{"role":"user","content":"hi"}]})
        }

        #[tokio::test]
        async fn effort_rewrites_the_model_tier() {
            assert_eq!(
                complete_model(chat_body(json!("max")), "swe-2-high").await,
                "swe-2-max"
            );
            assert_eq!(
                complete_model(chat_body(json!("low")), "swe-2-flash-high").await,
                "swe-2-flash-medium"
            );
            assert_eq!(
                complete_model(chat_body(json!("minimal")), "swe-2-medium-lightning").await,
                "swe-2-medium-lightning"
            );
            assert_eq!(
                complete_model(chat_body(json!("bogus")), "swe-2-high").await,
                "swe-2-high"
            );
            assert_eq!(
                complete_model(chat_body(json!("max")), "swe-2").await,
                "swe-2"
            );
        }

        #[tokio::test]
        async fn absent_effort_keeps_the_deployment_model() {
            let body = json!({"model":"swe-2",
                "messages":[{"role":"user","content":"hi"}]});
            assert_eq!(complete_model(body, "swe-2-high").await, "swe-2-high");
        }

        fn tools_body() -> Value {
            json!({"model":"swe-2",
                "messages":[{"role":"user","content":"first line of /etc/hostname?"}],
                "tools":[{"type":"function","function":{"name":"read",
                    "description":"Read a file","parameters":{"type":"object",
                    "properties":{"path":{"type":"string"}},"required":["path"]}}}]})
        }

        async fn serve_body(body: Value, reply: &str) -> Value {
            let dir = tempfile::tempdir().unwrap();
            let adapter = DevinAcp::new(script(dir.path(), reply));
            let GatewayResponse::Complete { body: out, .. } = adapter
                .serve(&request(body), &dep("swe-2-high"))
                .await
                .unwrap()
            else {
                panic!("expected a complete response");
            };
            serde_json::from_slice(&out).unwrap()
        }

        #[tokio::test]
        async fn tools_reply_is_emitted_as_openai_tool_calls() {
            let out = serve_body(
                tools_body(),
                r#"{"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":{"path":"/etc/hostname"}}}]}"#,
            )
            .await;
            let message = &out["choices"][0]["message"];
            assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
            assert!(message["content"].is_null());
            assert_eq!(message["tool_calls"][0]["id"], "call_1");
            assert_eq!(message["tool_calls"][0]["function"]["name"], "read");
            // Object-shaped arguments are normalized to the wire string form.
            assert_eq!(
                message["tool_calls"][0]["function"]["arguments"],
                r#"{"path":"/etc/hostname"}"#
            );
        }

        #[tokio::test]
        async fn plain_text_reply_with_tools_stays_text() {
            let out = serve_body(tools_body(), "a1-free-2").await;
            assert_eq!(out["choices"][0]["finish_reason"], "stop");
            assert_eq!(out["choices"][0]["message"]["content"], "a1-free-2");
            assert!(out["choices"][0]["message"]["tool_calls"].is_null());
        }

        #[tokio::test]
        async fn tool_schemas_reach_the_prompt() {
            let dir = tempfile::tempdir().unwrap();
            let adapter = DevinAcp::new(script(dir.path(), "ok"));
            adapter
                .serve(&request(tools_body()), &dep("swe-2-high"))
                .await
                .unwrap();
            let log = std::fs::read_to_string(dir.path().join(".local/share/prompts.log")).unwrap();
            assert!(log.contains(r#""name\":\"read\""#), "{log}");
        }

        #[tokio::test]
        async fn tool_choice_none_skips_the_tool_protocol() {
            let dir = tempfile::tempdir().unwrap();
            let adapter = DevinAcp::new(script(dir.path(), "ok"));
            let mut body = tools_body();
            body["tool_choice"] = json!("none");
            adapter
                .serve(&request(body), &dep("swe-2-high"))
                .await
                .unwrap();
            let log = std::fs::read_to_string(dir.path().join(".local/share/prompts.log")).unwrap();
            assert!(!log.contains("Tools"), "{log}");
        }

        #[tokio::test]
        async fn sessions_run_in_ask_mode_before_the_prompt() {
            let dir = tempfile::tempdir().unwrap();
            let adapter = DevinAcp::new(script(dir.path(), "ok"));
            adapter
                .serve(&request(chat_body(Value::Null)), &dep("swe-2-high"))
                .await
                .unwrap();
            let log =
                std::fs::read_to_string(dir.path().join(".local/share/requests.log")).unwrap();
            let set_mode = log.find(r#""session/set_mode""#).expect("set_mode call");
            assert!(log.contains(r#""modeId":"ask""#), "{log}");
            assert!(set_mode < log.find(r#""session/prompt""#).unwrap(), "{log}");
        }

        #[tokio::test]
        async fn image_parts_reach_the_agent_as_files() {
            let dir = tempfile::tempdir().unwrap();
            let adapter = DevinAcp::new(script(dir.path(), "a cat"));
            let body = json!({"model":"swe-2","messages":[
            {"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}
            ]}]});
            adapter
                .serve(&request(body), &dep("swe-2-high"))
                .await
                .unwrap();
            let log = std::fs::read_to_string(dir.path().join(".local/share/prompts.log")).unwrap();
            assert!(log.contains("[image 1]"), "{log}");
            assert!(log.contains("[Images]"), "{log}");
            assert!(
                log.contains("image 1: ") && log.contains(".llmux-images"),
                "{log}"
            );
            // The staged file lives only for the turn: after the reply, the
            // staging directory is gone even though the parent may remain.
            let leftovers = std::fs::read_dir(dir.path().join(".llmux-images"))
                .map(|d| d.count())
                .unwrap_or(0);
            assert_eq!(leftovers, 0);
        }

        #[tokio::test]
        async fn permission_requests_are_cancelled_not_errored() {
            let dir = tempfile::tempdir().unwrap();
            let config = script(dir.path(), "done");
            std::fs::write(dir.path().join(".local/share/ask_permission"), "").unwrap();
            let adapter = DevinAcp::new(config);
            let GatewayResponse::Complete { body, .. } = adapter
                .serve(&request(chat_body(Value::Null)), &dep("swe-2-high"))
                .await
                .unwrap()
            else {
                panic!("expected a complete response");
            };
            let reply_log =
                std::fs::read_to_string(dir.path().join(".local/share/perm_reply.log")).unwrap();
            assert!(
                reply_log.contains(r#""outcome":"cancelled""#),
                "{reply_log}"
            );
            assert!(!reply_log.contains("-32601"), "{reply_log}");
            let out: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(out["choices"][0]["message"]["content"], "done");
        }

        #[tokio::test]
        async fn acp_usage_is_reported_on_the_completion() {
            let dir = tempfile::tempdir().unwrap();
            let config = script(dir.path(), "ok");
            std::fs::write(dir.path().join(".local/share/with_usage"), "").unwrap();
            let adapter = DevinAcp::new(config);
            let GatewayResponse::Complete { body, .. } = adapter
                .serve(&request(chat_body(Value::Null)), &dep("swe-2-high"))
                .await
                .unwrap()
            else {
                panic!("expected a complete response");
            };
            let out: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(out["usage"]["prompt_tokens"], 11);
            assert_eq!(out["usage"]["completion_tokens"], 7);
            assert_eq!(out["usage"]["total_tokens"], 18);
            assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 5);
        }

        #[tokio::test]
        async fn streamed_tools_reply_yields_tool_calls_delta() {
            use futures_util::StreamExt;
            let dir = tempfile::tempdir().unwrap();
            let adapter = DevinAcp::new(script(
                dir.path(),
                r#"{"tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]}"#,
            ));
            let mut body = tools_body();
            let mut req = request(std::mem::take(&mut body));
            req.stream = true;
            let GatewayResponse::Stream(events) =
                adapter.serve(&req, &dep("swe-2-high")).await.unwrap()
            else {
                panic!("expected a stream");
            };
            let data: Vec<String> = events
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|e| e.unwrap().data)
                .collect();
            let parsed: Vec<Value> = data
                .iter()
                .filter(|d| *d != "[DONE]")
                .map(|d| serde_json::from_str(d).unwrap())
                .collect();
            let delta = parsed
                .iter()
                .find_map(|c| c["choices"][0]["delta"]["tool_calls"].as_array())
                .expect("a tool_calls delta");
            assert_eq!(delta[0]["function"]["name"], "read");
            assert_eq!(delta[0]["index"], 0);
            assert!(parsed
                .iter()
                .any(|c| c["choices"][0]["finish_reason"] == "tool_calls"));
        }

        async fn served_stream(mut body: Value, reply: &str, usage_marker: bool) -> Vec<String> {
            use futures_util::StreamExt;
            let dir = tempfile::tempdir().unwrap();
            let config = script(dir.path(), reply);
            if usage_marker {
                std::fs::write(dir.path().join(".local/share/with_usage"), "").unwrap();
            }
            let adapter = DevinAcp::new(config);
            let mut req = request(std::mem::take(&mut body));
            req.stream = true;
            let GatewayResponse::Stream(events) =
                adapter.serve(&req, &dep("swe-2-high")).await.unwrap()
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
        async fn a_buffered_tool_turn_still_opens_with_the_role_delta() {
            let data = served_stream(
                tools_body(),
                r#"{"tool_calls":[{"function":{"name":"read","arguments":"{}"}}]}"#,
                false,
            )
            .await;
            let first: Value = serde_json::from_str(&data[0]).unwrap();
            assert_eq!(
                first["choices"][0]["delta"],
                json!({"role":"assistant","content":""}),
                "{data:?}"
            );
        }

        #[tokio::test]
        async fn stream_usage_is_emitted_only_when_requested() {
            let mut body = tools_body();
            body["stream_options"] = json!({"include_usage":true});
            let data = served_stream(body, "ok", true).await;
            let usage = data
                .iter()
                .map(|d| serde_json::from_str::<Value>(d).unwrap())
                .find(|v| v["choices"].as_array().is_some_and(|c| c.is_empty()))
                .expect("a usage chunk");
            assert_eq!(usage["usage"]["prompt_tokens"], 11);
            assert_eq!(*data.last().unwrap(), "[DONE]");

            let data = served_stream(tools_body(), "ok", true).await;
            assert!(!data.iter().any(|d| d.contains(r#""usage""#)), "{data:?}");
        }

        fn script(dir: &std::path::Path, reply: &str) -> DevinAcpConfig {
            chunked_script(dir, &[reply])
        }

        /// Chunk notifications are serialized here, not in the shell script: serde
        /// escapes any reply correctly while printf's format-string pass does not.
        fn chunked_script(dir: &std::path::Path, replies: &[&str]) -> DevinAcpConfig {
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
            let path = dir.join("devin");
            let body = r#"#!/bin/sh
if [ "$1" = models ]; then printf '  swe-2-high  X  [Free]\n'; exit 0; fi
test -f "$XDG_DATA_HOME/marker" || exit 3
while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  printf '%s\n' "$line" >> "$XDG_DATA_HOME/requests.log"
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id";;
    *'"session/new"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s1"}}\n' "$id";;
    *'"session/set_mode"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id";;
    *'"session/prompt"'*)
      printf '%s\n' "$line" >> "$XDG_DATA_HOME/prompts.log"
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
            // Write under a private name, then rename into place: exec'ing a file another
            // thread still holds open for writing fails with ETXTBSY, and a rename is atomic.
            let staged = dir.join("devin.tmp");
            std::fs::write(&staged, body).unwrap();
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&staged, &path).unwrap();
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
                .turn(
                    &config,
                    "swe-2-high",
                    &[json!({"type":"text","text":"hi"})],
                    None,
                )
                .await;
            assert_eq!(text.unwrap().0, "OK");
        }

        async fn streamed(replies: &[&str]) -> Vec<String> {
            use futures_util::StreamExt;
            let dir = tempfile::tempdir().unwrap();
            let adapter = DevinAcp::new(chunked_script(dir.path(), replies));
            let GatewayResponse::Stream(events) = adapter.stream(
                "m".into(),
                vec![json!({"type":"text","text":"hi"})],
                None,
                false,
                false,
            ) else {
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
            let deltas = streamed(&["Hel", "lo ", "world"]).await;
            assert!(deltas.len() >= 2, "{deltas:?}");
            assert_eq!(deltas.concat(), "Hello world");
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
                .turn(&config, "m", &[json!("p")], None)
                .await
                .unwrap_err();
            assert!(matches!(
                err,
                UpstreamError::StreamBroken { .. } | UpstreamError::Connect
            ));
        }
    }
}
