//! Shared Agent Client Protocol plumbing: how the gateway spawns an agent CLI
//! as a managed child and talks JSON-RPC over its stdio. `devin_acp` is one
//! preset over this; `acp_agent` exposes it as a configurable provider.

use bytes::Bytes;
use http::StatusCode;
use llmux_types::UpstreamError;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
};

/// Cap on the text one turn may stream back: a child that never stops writing
/// must not grow memory without bound.
pub(crate) const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;

/// Builds the child command with a scrubbed environment: `HOME` and the XDG
/// dirs point at `home` so the CLI reads its own config/credentials from the
/// sandbox, `cwd` (defaulting to `home`) is the working directory, and the
/// parent's `PATH` is passed through so wrapper scripts find their runtime.
/// `env` is an explicit per-credential allowlist applied last — it may
/// override `PATH`/`LANG` but the config loader refuses `HOME`/`XDG_*` there,
/// so the sandbox home cannot be redirected from the credential.
pub(crate) fn cli_command(
    binary: &Path,
    args: &[String],
    home: &Path,
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> Command {
    let mut command = Command::new(binary);
    command
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("LANG", "C.UTF-8")
        .env("DISABLE_TELEMETRY", "1")
        .current_dir(cwd.unwrap_or(home))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    for (name, value) in env {
        command.env(name, value);
    }
    command
}

/// Spawns the CLI, retrying while the kernel reports the binary as busy.
///
/// Replacing a running binary in place leaves `ETXTBSY` on the next exec until the
/// last process holding it exits, and a freshly written script can be held the same
/// way by a child another thread forked mid-write. Neither is a real failure.
fn spawn_binary(command: &mut Command) -> Result<Child, UpstreamError> {
    let mut attempt = 0;
    loop {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return Err(UpstreamError::Connect),
        }
    }
}

/// Maps an ACP JSON-RPC error object onto the failover classes the executor
/// understands. Message text only — error payloads never carry credentials.
pub(crate) fn rpc_error(error: &Value) -> UpstreamError {
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

/// The `session/prompt` usage block mapped onto the OpenAI usage shape —
/// `prompt_tokens_details.cached_tokens` is the one detail field clients read.
pub(crate) fn usage_body(usage: Option<Value>) -> Option<Value> {
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

/// Synthesizes the OpenAI completion envelope the bridge produces locally;
/// `chatcmpl-<prefix>-<secs>` ids are gateway-generated and not unique per
/// request, so `upstream_request_id` reads headers only.
pub(crate) fn completion(prefix: &str, model: &str, text: &str, stream: bool) -> Value {
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let choice = if stream {
        json!({"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null})
    } else {
        json!({"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"})
    };
    json!({"id":format!("chatcmpl-{prefix}-{created}"),
        "object":if stream {"chat.completion.chunk"} else {"chat.completion"},
        "created":created,"model":model,"choices":[choice]})
}

/// Aborts the spawned turn task when the owning future is dropped — a
/// cancelled stream must not leave a turn running against a dead receiver.
pub(crate) struct AbortOnDrop<T>(pub tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Flattens one OpenAI `content` value to plain text: the string form, or the
/// `text` members of a parts array joined with newlines.
pub(crate) fn content_text(content: &Value) -> String {
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
pub(crate) enum ImageSource {
    Data { mime: String, data: String },
    Url(String),
}

/// Detects the image part spellings the bridge accepts: OpenAI `image_url` /
/// `input_image` (string or `{url}` object) and Anthropic `image` sources.
pub(crate) fn image_source(part: &Value) -> Option<ImageSource> {
    let url_field = |key: &str| {
        part.get(key)
            .and_then(|v| v.get("url").and_then(Value::as_str).or_else(|| v.as_str()))
    };
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

/// A `data:` URL carries its bytes inline; a remote reference must be
/// http(s) — anything else (a bare path, `file://`) would land in the
/// `[Images]` index and invite the agent to `read` a sandbox file it was
/// never meant to see.
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
    (url.starts_with("http://") || url.starts_with("https://"))
        .then(|| ImageSource::Url(url.to_owned()))
}

/// Decoded image bytes must not exceed this: agent CLIs accept inline `image`
/// blocks at the protocol level but do not show them to the model, so images
/// reach the model only as files the agent `read`s from its sandbox — and
/// unbounded writes are the failure to bound here.
pub(crate) const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// Removes the staging directory when the guard drops — including aborted
/// turns, since the future's locals are dropped with it.
#[derive(Debug)]
pub(crate) struct ImageStaging {
    pub(crate) dir: PathBuf,
}

impl Drop for ImageStaging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Per-request uniqueness for the staging dir: a nanosecond timestamp alone
/// can repeat on coarse clocks, so concurrent requests in this process would
/// share a dir and each guard's cleanup would delete the other's files.
static STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Writes each inline image under `<home>/.llmux-images/<unique>/` and returns
/// the transcript index entries — file paths for inline payloads, the URL
/// itself for remote ones. The ACP `image` content block is intentionally not
/// used: the agent acknowledges it but the model never sees its bytes, while
/// a file it `read`s is rendered to the model (probed live on 3000.14.30821).
pub(crate) fn stage_images(
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
    let unique = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
        STAGE_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let dir = home.join(".llmux-images").join(unique);
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
                // The encoded-length bound above is approximate; the exact cap
                // is enforced on the decoded bytes.
                if bytes.len() > MAX_IMAGE_BYTES {
                    return Err(UpstreamError::BadRequest {
                        status: StatusCode::PAYLOAD_TOO_LARGE,
                        body: Bytes::new(),
                    });
                }
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

/// Flattens chat messages into labelled `(role, text)` turns. Image parts
/// become `[image N]` position markers; an assistant turn that only made calls
/// still shows up as `[tool_calls: …]`; `tool` results keep their call id (and
/// name when present) so the model can pair call and result.
pub(crate) fn labelled_turns(messages: &[Value]) -> Vec<(String, String)> {
    let mut image_index = 0usize;
    messages
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
        .collect()
}

/// One JSON-RPC conversation with a spawned agent CLI. Speaks ACP: newline-
/// delimited `{"jsonrpc":"2.0",…}` frames in both directions, agent text
/// arriving as `session/update` notifications between our request and its
/// response. The child is killed on drop, and explicitly via [`Self::kill`]
/// once a one-shot turn ends.
pub(crate) struct AcpSession {
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

impl AcpSession {
    pub(crate) fn spawn(command: &mut Command) -> Result<Self, UpstreamError> {
        let mut child = spawn_binary(command)?;
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

    /// The child has exited on its own — a persistent session holding it must
    /// be respawned rather than reused.
    pub(crate) fn exited(&mut self) -> bool {
        self.child.try_wait().map(|s| s.is_some()).unwrap_or(true)
    }

    pub(crate) fn kill(&mut self) {
        let _ = self.child.start_kill();
    }

    /// The forwarder for streamed `agent_message_chunk` text — set per turn
    /// because each request owns a fresh channel.
    pub(crate) fn set_chunks(
        &mut self,
        chunks: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) {
        self.chunks = chunks;
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
                // Non-JSON noise (banners, progress chatter) is skipped, but a
                // `{`-led line that fails to parse is a broken frame — fail the
                // turn instead of waiting on a peer that cannot answer.
                if line.trim_start().starts_with('{') {
                    return Err(UpstreamError::Server {
                        status: StatusCode::BAD_GATEWAY,
                    });
                }
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
                        "result":{"outcome":"cancelled"}})
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

    /// The ACP handshake; once per child lifetime.
    pub(crate) async fn handshake(&mut self) -> Result<(), UpstreamError> {
        self.call(
            "initialize",
            json!({"protocolVersion":1,"clientInfo":{"name":"llmux","version":"1"},"clientCapabilities":{}}),
        )
        .await?;
        Ok(())
    }

    /// One chat turn: a fresh `session/new` in `ask` mode (the agent's own
    /// tools stay off agent-side — a structural bound, unlike prompt wording),
    /// then `session/prompt` collecting chunks until `end_turn`. Returns the
    /// collected reply text plus the turn usage when the agent reports it.
    ///
    /// The session's reply buffer and announce flag reset at the start, so a
    /// persistent session may carry any number of turns.
    pub(crate) async fn chat_turn(
        &mut self,
        cwd: &str,
        model: &str,
        prompt: &[Value],
    ) -> Result<(String, Option<Value>), UpstreamError> {
        self.reply.clear();
        self.announced = false;
        let session = self
            .call(
                "session/new",
                json!({"cwd":cwd,"mcpServers":[],"model":model}),
            )
            .await?;
        let session_id = session["sessionId"].as_str().ok_or(UpstreamError::Server {
            status: StatusCode::BAD_GATEWAY,
        })?;
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
        if result["stopReason"] != "end_turn" {
            return Err(UpstreamError::Server {
                status: StatusCode::BAD_GATEWAY,
            });
        }
        Ok((
            std::mem::take(&mut self.reply),
            result.get("usage").filter(|u| u.is_object()).cloned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_allowlist_overrides_passthrough_but_not_sandbox() {
        let home = Path::new("/sandbox");
        let env = BTreeMap::from([
            ("LANG".to_owned(), "en_US.UTF-8".to_owned()),
            ("AGENT_FLAG".to_owned(), "on".to_owned()),
        ]);
        let command = cli_command(
            Path::new("/bin/true"),
            &["--acp".to_owned()],
            home,
            None,
            &env,
        );
        let command = command.as_std();
        let env = |name: &str| {
            command
                .get_envs()
                .find(|(k, _)| *k == std::ffi::OsStr::new(name))
                .and_then(|(_, v)| v)
                .map(|v| v.to_owned())
        };
        assert_eq!(
            env("LANG").as_deref(),
            Some(std::ffi::OsStr::new("en_US.UTF-8"))
        );
        assert_eq!(
            env("AGENT_FLAG").as_deref(),
            Some(std::ffi::OsStr::new("on"))
        );
        assert_eq!(
            env("HOME").as_deref(),
            Some(std::ffi::OsStr::new("/sandbox"))
        );
        assert_eq!(
            env("XDG_DATA_HOME").as_deref(),
            Some(std::ffi::OsStr::new("/sandbox/.local/share"))
        );
        assert_eq!(command.get_current_dir(), Some(home));
    }
}
