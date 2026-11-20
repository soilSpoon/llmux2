use crate::{http_client::client, process::Directory, Result};
use futures_util::stream;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, StreamBody};
use hyper::{
    body::{Bytes, Frame, Incoming},
    service::service_fn,
    Request, Response,
};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    net::TcpListener,
    task::{JoinHandle, JoinSet},
};

#[cfg(not(test))]
type GatewayServer = crate::process::Server;
#[cfg(test)]
type GatewayServer = llmux_testkit::TestServer;

fn deployment_timeout(normal_ms: u64) -> u64 {
    if cfg!(test) {
        100
    } else {
        normal_ms
    }
}

type Error = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, Error>;
const PROMPT: &str = "Reply with the single word OK.";
const HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "host",
    "content-length",
    "accept-encoding",
];

fn parse_config(text: &str) -> Result<Value> {
    Ok(serde_json::to_value(toml::from_str::<toml::Value>(text)?)?)
}
fn render_config(config: &Value) -> Result<String> {
    Ok(toml::to_string(config)?)
}
#[derive(Clone)]
struct Target {
    name: String,
    provider: String,
    base_url: String,
    key_ref: String,
    upstream_model: String,
}
impl Target {
    fn describe(&self) -> String {
        format!(
            "{} ({}, {})",
            self.host(),
            self.provider,
            self.upstream_model
        )
    }
    fn host(&self) -> String {
        url::Url::parse(&self.base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default()
    }
}
fn targets_from_config(config: &Value) -> Result<Vec<Target>> {
    let credentials: Vec<_> = config["credentials"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    let mut targets: Vec<Target> = Vec::new();
    for model in config["models"].as_array().into_iter().flatten() {
        for deployment in model["deployments"].as_array().into_iter().flatten() {
            let Some(credential) = credentials
                .iter()
                .find(|c| c["id"] == deployment["credential"] && c["kind"] == "api_key")
            else {
                continue;
            };
            let base = deployment["base_url"]
                .as_str()
                .ok_or("missing base URL")?
                .trim_end_matches('/');
            if targets.iter().any(|t| t.base_url == base) {
                continue;
            }
            let host = url::Url::parse(base)?
                .host_str()
                .ok_or("missing host")?
                .to_owned();
            let name = host
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("-");
            targets.push(Target {
                name,
                provider: credential["provider"]
                    .as_str()
                    .ok_or("missing provider")?
                    .into(),
                base_url: base.into(),
                key_ref: credential["key"].as_str().ok_or("missing key")?.into(),
                upstream_model: deployment["upstream_model"]
                    .as_str()
                    .ok_or("missing upstream model")?
                    .into(),
            });
        }
    }
    Ok(targets)
}
fn redact(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_owned();
    for value in secrets.iter().filter(|v| v.len() >= 8) {
        text = text.replace(value, "[REDACTED]");
    }
    let mut out = String::new();
    let mut index = 0;
    while index < text.len() {
        let rest = &text[index..];
        if rest
            .get(..6)
            .is_some_and(|s| s.eq_ignore_ascii_case("bearer"))
        {
            let suffix = &rest[6..];
            if suffix.starts_with(char::is_whitespace) {
                let start = rest.len() - suffix.trim_start().len();
                let len = rest[start..]
                    .find(|c: char| !c.is_ascii_alphanumeric() && !"._~+/=-".contains(c))
                    .unwrap_or(rest.len() - start);
                if len >= 8 {
                    out.push_str(&rest[..start]);
                    out.push_str("[REDACTED]");
                    index += start + len;
                    continue;
                }
            }
        }
        let c = rest.chars().next().expect("nonempty");
        out.push(c);
        index += c.len_utf8();
    }
    out
}
fn markdown(report: &Value) -> String {
    let text = |v: &Value| {
        v.as_str().map(str::to_owned).unwrap_or_else(|| {
            if v.is_null() {
                "None".into()
            } else {
                v.to_string()
            }
        })
    };
    let mut out = format!(
        "# llmux2 live reliability run\n\n- primary: {}\n- fallback: {}\n- binary: {}\n\n",
        text(&report["primary"]),
        text(&report["fallback"]),
        text(&report["binary"])
    );
    if report["probes"].as_array().is_some_and(|a| !a.is_empty()) {
        out+="## Provider probes\n\n| target | healthy | observed | upstream status |\n| --- | --- | --- | --- |\n";
        for row in report["probes"].as_array().into_iter().flatten() {
            out += &format!(
                "| {} | {} | {} | {} |\n",
                text(&row["target"]),
                if row["healthy"] == true { "yes" } else { "no" },
                text(&row["observed"]).replace('|', "/"),
                text(&row["upstream"])
            );
        }
        out.push('\n');
    }
    let mut counts = BTreeMap::<String, usize>::new();
    for row in report["results"].as_array().into_iter().flatten() {
        *counts.entry(text(&row["verdict"])).or_default() += 1;
    }
    out+=&format!("## Scenarios ({})\n\n| verdict | scenario | expected | observed |\n| --- | --- | --- | --- |\n",counts.iter().map(|(k,v)|format!("{v} {k}")).collect::<Vec<_>>().join(", "));
    for row in report["results"].as_array().into_iter().flatten() {
        let observed = if row["observed"].as_str().is_some_and(|s| !s.is_empty()) {
            &row["observed"]
        } else {
            &row["note"]
        };
        out += &format!(
            "| {} | {} | {} | {} |\n",
            text(&row["verdict"]),
            text(&row["scenario"]),
            text(&row["expect"]),
            text(observed).replace('|', "/")
        );
    }
    out
}
fn random_token() -> Result<String> {
    let mut bytes = [0; 24];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn full(data: impl Into<Bytes>) -> Body {
    Full::new(data.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}
fn json_response(status: u16, payload: Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full(payload.to_string()))
        .expect("valid response")
}
#[derive(Clone)]
struct Route(Arc<Mutex<RouteState>>);
struct RouteState {
    upstream: String,
    fault: String,
    options: Value,
    hits: usize,
    statuses: Vec<Value>,
}
impl Route {
    fn hits(&self) -> usize {
        self.0.lock().expect("route lock").hits
    }
    fn statuses(&self) -> Vec<Value> {
        self.0.lock().expect("route lock").statuses.clone()
    }
    fn fault(&self, fault: &str) {
        self.0.lock().expect("route lock").fault = fault.into();
    }
    fn record(&self, value: Value) {
        self.0.lock().expect("route lock").statuses.push(value);
    }
}
struct Auth {
    refresh: String,
    tokens: BTreeSet<String>,
    fail: bool,
    grants: usize,
}
struct State {
    routes: Mutex<Vec<Route>>,
    fake_hits: Mutex<usize>,
    auth: Mutex<Auth>,
}
struct Lab {
    address: String,
    state: Arc<State>,
    task: JoinHandle<()>,
}
impl Drop for Lab {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Lab {
    async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let state = Arc::new(State {
            routes: Mutex::new(vec![]),
            fake_hits: Mutex::new(0),
            auth: Mutex::new(Auth {
                refresh: random_token()?,
                tokens: BTreeSet::new(),
                fail: false,
                grants: 0,
            }),
        });
        let inner = state.clone();
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    socket=listener.accept()=>{
                        let Ok((socket,_))=socket else {break;};
                        let state=inner.clone();
                        tasks.spawn(async move {let _=serve(socket,state).await;});
                    },
                    _=tasks.join_next(),if !tasks.is_empty()=>{}
                }
            }
        });
        Ok(Self {
            address,
            state,
            task,
        })
    }
    fn fake_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
    fn route(&self, upstream: &str, fault: &str, options: Value) -> (String, Route) {
        let route = Route(Arc::new(Mutex::new(RouteState {
            upstream: upstream.trim_end_matches('/').into(),
            fault: fault.into(),
            options,
            hits: 0,
            statuses: vec![],
        })));
        let mut routes = self.state.routes.lock().expect("routes lock");
        let index = routes.len();
        routes.push(route.clone());
        (format!("http://{}/r/{index}", self.address), route)
    }
    #[cfg(test)]
    fn fake_hits(&self) -> usize {
        *self.state.fake_hits.lock().expect("fake lock")
    }
    fn token_url(&self) -> String {
        format!("http://{}/token", self.address)
    }
}
async fn serve(socket: tokio::net::TcpStream, state: Arc<State>) -> Result<()> {
    let socket = socket.into_std()?;
    let control = Arc::new(tokio::net::TcpSocket::from_std_stream(socket.try_clone()?));
    let socket = tokio::net::TcpStream::from_std(socket)?;
    hyper::server::conn::http1::Builder::new()
        .serve_connection(
            TokioIo::new(socket),
            service_fn(move |request| handle(state.clone(), control.clone(), request)),
        )
        .await?;
    Ok(())
}
fn reset_socket(socket: &tokio::net::TcpSocket) -> std::io::Result<()> {
    #[allow(
        deprecated,
        reason = "Zero-duration linger sends RST without blocking on pending data"
    )]
    socket.set_linger(Some(Duration::ZERO))
}
async fn handle(
    state: Arc<State>,
    socket: Arc<tokio::net::TcpSocket>,
    request: Request<Incoming>,
) -> Result<Response<Body>> {
    let path = request.uri().path().to_owned();
    let (parts, body) = request.into_parts();
    let body = body.collect().await?.to_bytes();
    if path == "/token" {
        let form: BTreeMap<_, _> = url::form_urlencoded::parse(&body).into_owned().collect();
        let mut auth = state.auth.lock().expect("auth lock");
        if auth.fail
            || form.get("grant_type").map(String::as_str) != Some("refresh_token")
            || form.get("refresh_token") != Some(&auth.refresh)
        {
            return Ok(json_response(400, json!({"error":"invalid_grant"})));
        }
        let token = random_token()?;
        auth.tokens.insert(token.clone());
        auth.grants += 1;
        return Ok(json_response(
            200,
            json!({"access_token":token,"token_type":"Bearer","expires_in":3600}),
        ));
    }
    if path.starts_with("/v1/") {
        *state.fake_hits.lock().expect("fake lock") += 1;
        if parts
            .headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            != Some("Bearer offline-key")
        {
            return Ok(json_response(
                401,
                json!({"error":{"message":"Incorrect API key","type":"invalid_request_error","code":"invalid_api_key"}}),
            ));
        }
        let data: Value = serde_json::from_slice(&body)?;
        let model = data.get("model").cloned().unwrap_or(json!("fake"));
        if path == "/v1/responses" && data["stream"] != true {
            return Ok(json_response(
                200,
                json!({"id":"r","object":"response","model":model,"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"OK"}]}]}),
            ));
        }
        if data["stream"] == true {
            let mut events = Vec::new();
            for (i, word) in ["O", "K", ".", "", ""].iter().enumerate() {
                events.push(format!("data: {}\n\n",json!({"id":"c","object":"chat.completion.chunk","model":model,"choices":[{"index":0,"delta":if i<3 {json!({"content":word})}else{json!({})},"finish_reason":if i==4 {json!("stop")}else{Value::Null}}]})));
            }
            events.push("data: [DONE]\n\n".into());
            let body = StreamBody::new(stream::unfold(events.into_iter(), |mut events| async {
                let event = events.next()?;
                tokio::time::sleep(Duration::from_millis(if cfg!(test) { 1 } else { 50 })).await;
                Some((Ok::<_, Error>(Frame::data(Bytes::from(event))), events))
            }))
            .boxed_unsync();
            return Ok(Response::builder()
                .header("content-type", "text/event-stream")
                .body(body)?);
        }
        return Ok(json_response(
            200,
            json!({"id":"c","object":"chat.completion","model":model,"choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}}),
        ));
    }
    let Some((index, suffix)) = path.strip_prefix("/r/").and_then(|s| s.split_once('/')) else {
        return Ok(json_response(
            404,
            json!({"error":{"message":"unknown route"}}),
        ));
    };
    let route = index
        .parse::<usize>()
        .ok()
        .and_then(|i| state.routes.lock().expect("routes lock").get(i).cloned());
    let Some(route) = route else {
        return Ok(json_response(
            404,
            json!({"error":{"message":"unknown route"}}),
        ));
    };
    let (upstream, fault, options) = {
        let mut r = route.0.lock().expect("route lock");
        r.hits += 1;
        (r.upstream.clone(), r.fault.clone(), r.options.clone())
    };
    match fault.as_str() {
        "status" => {
            let status = options["status"].as_u64().ok_or("missing status")? as u16;
            route.record(json!(status));
            let mut response = json_response(
                status,
                options
                    .get("body")
                    .cloned()
                    .unwrap_or(json!({"error":{"message":"injected"}})),
            );
            for (k, v) in options["headers"].as_object().into_iter().flatten() {
                response.headers_mut().insert(
                    hyper::header::HeaderName::from_bytes(k.as_bytes())?,
                    v.as_str().ok_or("invalid header")?.parse()?,
                );
            }
            return Ok(response);
        }
        "hang" => {
            route.record(json!("hang"));
            tokio::time::sleep(Duration::from_secs(
                options["seconds"].as_u64().unwrap_or(120),
            ))
            .await;
            reset_socket(&socket)?;
            return Err("injected hang".into());
        }
        "reset" => {
            route.record(json!("reset"));
            reset_socket(&socket)?;
            return Err("injected reset".into());
        }
        "sse_then_close" => {
            route.record(json!(200));
            let body = StreamBody::new(stream::once(async move {
                tokio::time::sleep(Duration::from_millis(if cfg!(test) { 2 } else { 200 })).await;
                reset_socket(&socket)?;
                Err::<Frame<Bytes>, Error>("injected close".into())
            }))
            .boxed_unsync();
            return Ok(Response::builder()
                .header("content-type", "text/event-stream")
                .body(body)?);
        }
        _ => {}
    }
    let mut headers = parts.headers;
    if fault == "oauth" {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        let valid = state.auth.lock().expect("auth lock").tokens.contains(token);
        if !valid {
            route.record(json!(401));
            return Ok(json_response(
                401,
                json!({"error":{"message":"token expired or revoked","type":"invalid_request_error","code":"invalid_api_key"}}),
            ));
        }
        headers.remove("x-api-key");
        headers.insert(
            "authorization",
            format!(
                "Bearer {}",
                options["real_key"].as_str().ok_or("missing real key")?
            )
            .parse()?,
        );
    }
    let mut request = Request::builder()
        .method(parts.method)
        .uri(format!("{upstream}/{suffix}"))
        .header("accept-encoding", "identity");
    for (k, v) in &headers {
        if !HOP.contains(&k.as_str()) {
            request = request.header(k, v);
        }
    }
    let response = match tokio::time::timeout(
        Duration::from_secs(options["upstream_timeout"].as_u64().unwrap_or(120)),
        client(&upstream)?.request(request.body(Full::new(body))?),
    )
    .await
    {
        Ok(Ok(r)) => r,
        _ => {
            route.record(json!("upstream-error"));
            return Err("upstream failure".into());
        }
    };
    route.record(json!(response.status().as_u16()));
    let (parts, incoming) = response.into_parts();
    let mut response = Response::builder().status(parts.status);
    for (k, v) in &parts.headers {
        if !HOP.contains(&k.as_str()) {
            response = response.header(k, v);
        }
    }
    let streaming = parts
        .headers
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|s| s.contains("text/event-stream"));
    if !streaming {
        return Ok(response.body(full(incoming.collect().await?.to_bytes()))?);
    }
    let cutoff = if ["cut", "stall"].contains(&fault.as_str()) {
        options["cut_after_events"].as_u64().map(|n| n as usize)
    } else {
        None
    };
    let body = StreamBody::new(stream::unfold(
        (incoming, 0usize, false, false, false),
        move |(mut incoming, mut events, mut newline, stopped, finished)| {
            let fault = fault.clone();
            let socket = socket.clone();
            let seconds = options["seconds"].as_u64().unwrap_or(120);
            async move {
                if finished {
                    return None;
                }
                if stopped {
                    if fault == "stall" {
                        tokio::time::sleep(Duration::from_secs(seconds)).await;
                    }
                    let _ = reset_socket(&socket);
                    return Some((
                        Err::<Frame<Bytes>, Error>("injected stream cut".into()),
                        (incoming, events, newline, false, true),
                    ));
                }
                loop {
                    let frame = incoming.frame().await?;
                    match frame {
                        Err(e) => {
                            return Some((Err(e.into()), (incoming, events, newline, false, true)))
                        }
                        Ok(frame) => {
                            if let Ok(data) = frame.into_data() {
                                let mut length = data.len();
                                let mut stop = false;
                                for (i, b) in data.iter().enumerate() {
                                    if *b == b'\n' && newline {
                                        events += 1;
                                    }
                                    if *b != b'\r' {
                                        newline = *b == b'\n';
                                    }
                                    if cutoff.is_some_and(|cut| events >= cut) {
                                        length = i + 1;
                                        stop = true;
                                        break;
                                    }
                                }
                                return Some((
                                    Ok(Frame::data(data.slice(..length))),
                                    (incoming, events, newline, stop, false),
                                ));
                            }
                        }
                    }
                }
            }
        },
    ))
    .boxed_unsync();
    Ok(response.body(body)?)
}

struct Reply {
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
    seconds: f64,
    stream: bool,
}
impl Reply {
    fn deployment(&self) -> &str {
        self.headers
            .get("x-llmux-deployment")
            .map(String::as_str)
            .unwrap_or("-")
    }
    fn attempts(&self) -> usize {
        self.headers
            .get("x-llmux-attempts")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }
    fn done(&self) -> bool {
        self.body.lines().any(|l| {
            l.strip_prefix("data:")
                .is_some_and(|s| s.trim() == "[DONE]")
        }) || self.body.contains("response.completed")
    }
    fn broken(&self) -> bool {
        self.body.contains("stream_broken")
    }
    fn events(&self) -> usize {
        self.body
            .lines()
            .filter(|l| {
                l.starts_with("data:") && !l.contains("[DONE]") && !l.contains("stream_broken")
            })
            .count()
    }
    fn summary(&self, secrets: &[String]) -> String {
        let mut s = format!(
            "{} dep={} attempts={} {:.1}s",
            self.status,
            self.deployment(),
            if self.attempts() == 0 {
                "-".into()
            } else {
                self.attempts().to_string()
            },
            self.seconds
        );
        if self.stream {
            s += &format!(
                " events={} done={} broken={}",
                self.events(),
                self.done(),
                self.broken()
            );
        }
        if self.status != 200 {
            s.push(' ');
            s += &redact(&self.body, secrets)
                .chars()
                .take(160)
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
        }
        s
    }
}
async fn request(url: &str, key: &str, payload: Value) -> Result<Reply> {
    let start = Instant::now();
    let mut reply = Reply {
        status: 0,
        headers: BTreeMap::new(),
        body: String::new(),
        seconds: 0.0,
        stream: payload["stream"] == true,
    };
    let operation = async {
        let response = client(url)?
            .request(
                Request::post(url)
                    .header("authorization", format!("Bearer {key}"))
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(payload.to_string())))?,
            )
            .await?;
        reply.status = response.status().as_u16();
        reply.headers = response
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_owned())))
            .collect();
        let mut body = response.into_body();
        let mut raw = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame?;
            if let Some(data) = frame.data_ref() {
                raw.extend_from_slice(data);
                if raw.len() > 1024 * 1024 {
                    return Err("response exceeds limit".into());
                }
                reply.body = String::from_utf8_lossy(&raw).into_owned();
            }
        }
        Ok::<_, Error>(())
    };
    let _ = tokio::time::timeout(Duration::from_secs(150), operation).await;
    reply.seconds = start.elapsed().as_secs_f64();
    Ok(reply)
}
struct Gateway {
    server: GatewayServer,
    _directory: Directory,
    key: String,
}
impl Gateway {
    async fn start(binary: &Path, mut config: Value) -> Result<Self> {
        let directory = Directory::new()?;
        let key = random_token()?;
        let path = directory.0.join("client.key");
        fs::write(&path, &key)?;
        config["server"]["listen"] = json!("127.0.0.1:0");
        config["server"]["client_keys"] = json!([format!("file:{}", path.display())]);
        let path = directory.0.join("llmux.toml");
        let text = render_config(&config)?;
        fs::write(&path, &text)?;
        #[cfg(test)]
        let server = {
            let _ = binary;
            GatewayServer::start(
                llmux_server::app(llmux_config::RuntimeConfig::parse(&text)?).await?,
            )
            .await?
        };
        #[cfg(not(test))]
        let binary = binary.to_owned();
        #[cfg(not(test))]
        let server = tokio::task::spawn_blocking(move || {
            GatewayServer::start(&binary, &[path.to_string_lossy().into_owned()], 2)
        })
        .await??;
        Ok(Self {
            server,
            _directory: directory,
            key,
        })
    }
    async fn request(&self, model: &str, stream: bool, responses: bool) -> Result<Reply> {
        let path = if responses {
            "responses"
        } else {
            "chat/completions"
        };
        let mut payload = json!({"model":model,"stream":stream});
        if responses {
            payload["input"] = json!(PROMPT);
            payload["max_output_tokens"] = json!(16);
        } else {
            payload["messages"] = json!([{"role":"user","content":PROMPT}]);
            payload["max_tokens"] = json!(16);
        }
        #[cfg(not(test))]
        let base = format!("http://{}", self.server.address);
        #[cfg(test)]
        let base = self.server.url();
        request(&format!("{base}/v1/{path}"), &self.key, payload).await
    }
}

struct Suite<'a> {
    binary: &'a Path,
    primary: &'a Target,
    fallback: &'a Target,
    lab: &'a Lab,
    secrets: &'a [String],
    results: Vec<Value>,
}
impl Suite<'_> {
    fn check(&mut self, name: &str, expect: &str, ok: bool, observed: String) {
        let verdict = if ok { "PASS" } else { "FAIL" };
        let observed = redact(&observed, self.secrets);
        println!("{verdict:<4} {name:<34} {observed}");
        self.results.push(json!({"scenario":name,"expect":expect,"verdict":verdict,"observed":observed,"note":""}));
    }
    fn skip(&mut self, name: &str, expect: &str, note: &str) {
        println!("SKIP {name:<34} {note}");
        self.results.push(
            json!({"scenario":name,"expect":expect,"verdict":"SKIP","observed":"","note":note}),
        );
    }
    fn deployment(
        &self,
        id: &str,
        target: &Target,
        priority: usize,
        base: &str,
        extra: Value,
    ) -> (Value, Value) {
        let credential = format!("cred-{id}");
        let mut dep = json!({"id":id,"credential":credential,"base_url":base,"upstream_model":target.upstream_model,"priority":priority});
        for (k, v) in extra.as_object().into_iter().flatten() {
            dep[k] = v.clone();
        }
        (
            dep,
            json!({"id":credential,"kind":"api_key","provider":target.provider,"key":target.key_ref}),
        )
    }
    async fn gateway(
        &self,
        deployments: Vec<Value>,
        credentials: Vec<Value>,
        models: Option<Value>,
    ) -> Result<Gateway> {
        Gateway::start(self.binary,json!({"server":{"request_timeout_ms":60000},"routing":{"max_attempts":3},"cooldown":{"default_ms":5000,"max_ms":60000},"credentials":credentials,"models":models.unwrap_or(json!([{"name":"rel","deployments":deployments}]))})).await
    }
    async fn pair(
        &self,
        fault: &str,
        options: Value,
        extra: Value,
    ) -> Result<(Gateway, Route, Route)> {
        let (url, p) = self.lab.route(&self.primary.base_url, fault, options);
        let (furl, f) = self.lab.route(&self.fallback.base_url, "pass", json!({}));
        let (pd, pc) = self.deployment("primary", self.primary, 0, &url, extra);
        let (fd, fc) = self.deployment("fallback", self.fallback, 1, &furl, json!({}));
        Ok((self.gateway(vec![pd, fd], vec![pc, fc], None).await?, p, f))
    }
    async fn failover(
        &mut self,
        name: &str,
        expect: &str,
        fault: &str,
        options: Value,
        extra: Value,
        mode: (bool, bool),
    ) -> Result<(Gateway, Route, Route)> {
        let (stream, responses) = mode;
        let (gw, p, f) = self.pair(fault, options, extra).await?;
        let r = gw.request("rel", stream, responses).await?;
        self.check(
            name,
            expect,
            r.status == 200
                && r.deployment() == "fallback"
                && p.hits() >= 1
                && f.hits() == 1
                && (!stream || r.done() && !r.broken()),
            format!(
                "{} primary_upstream={:?} fallback_upstream={:?}",
                r.summary(self.secrets),
                p.statuses(),
                f.statuses()
            ),
        );
        Ok((gw, p, f))
    }
    async fn bad_key(&mut self, target: &Target, name: &str) -> Result<()> {
        let directory = Directory::new()?;
        let path = directory.0.join("badkey");
        fs::write(
            &path,
            format!("sk-llmux-reliability-invalid-{}", random_token()?),
        )?;
        let (url, p) = self.lab.route(&target.base_url, "pass", json!({}));
        let (furl, f) = self.lab.route(&self.fallback.base_url, "pass", json!({}));
        let (pd, mut pc) = self.deployment("primary", target, 0, &url, json!({}));
        pc["key"] = json!(format!("file:{}", path.display()));
        let (fd, fc) = self.deployment("fallback", self.fallback, 1, &furl, json!({}));
        let gw = self.gateway(vec![pd, fd], vec![pc, fc], None).await?;
        let r = gw.request("rel", false, false).await?;
        self.check(
            &format!("{name}:{}", target.name),
            &format!("invalid key at {} -> fallback", target.describe()),
            r.status == 200 && r.deployment() == "fallback" && f.hits() == 1,
            format!(
                "{} primary_upstream={:?}",
                r.summary(self.secrets),
                p.statuses()
            ),
        );
        let hits = p.hits();
        let r = gw.request("rel", false, false).await?;
        self.check(
            &format!("{name}_disabled:{}", target.name),
            "rejected credential is not retried on the next request",
            r.status == 200 && r.deployment() == "fallback" && p.hits() == hits,
            format!(
                "{} primary_hits={}",
                r.summary(self.secrets),
                p.hits() - hits
            ),
        );
        Ok(())
    }
    async fn run_all(&mut self) -> Result<()> {
        let retry_after = if cfg!(test) { "1" } else { "4" };
        let (gw,p,_)=self.failover("rate_limit_429_failover","429 + Retry-After on primary -> served by fallback, attempts 2","status",json!({"status":429,"headers":{"Retry-After":retry_after},"body":{"error":{"message":"Rate limit reached for requests","type":"requests","code":"rate_limit_exceeded"}}}),json!({}),(false,false)).await?;
        let hits = p.hits();
        let r = gw.request("rel", false, false).await?;
        self.check(
            "rate_limit_cooldown_skips_primary",
            "next request inside Retry-After skips primary without contacting it",
            r.status == 200
                && r.deployment() == "fallback"
                && r.attempts() == 1
                && p.hits() == hits,
            format!(
                "{} primary_hits={}",
                r.summary(self.secrets),
                p.hits() - hits
            ),
        );
        p.fault("pass");
        tokio::time::sleep(Duration::from_millis(if cfg!(test) { 1100 } else { 4500 })).await;
        let r = gw.request("rel", false, false).await?;
        self.check(
            "rate_limit_cooldown_expires",
            "after Retry-After the primary is used again",
            r.status == 200 && r.deployment() == "primary",
            r.summary(self.secrets),
        );
        drop(gw);
        for (name, status, body) in [
            (
                "quota_429_insufficient_quota",
                429,
                json!({"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}),
            ),
            (
                "quota_402_payment_required",
                402,
                json!({"error":{"message":"Insufficient credits","code":402}}),
            ),
        ] {
            let (gw, p, _) = self
                .failover(
                    name,
                    "quota error on primary -> fallback, primary cooled down",
                    "status",
                    json!({"status":status,"body":body}),
                    json!({}),
                    (false, false),
                )
                .await?;
            let hits = p.hits();
            let r = gw.request("rel", false, false).await?;
            self.check(
                &format!("{name}_cooldown"),
                "quota cooldown keeps primary out of rotation",
                r.status == 200 && r.deployment() == "fallback" && p.hits() == hits,
                r.summary(self.secrets),
            );
        }
        let (gw, p, _) = self
            .pair("pass", json!({}), json!({"quota":{"rpm":1}}))
            .await?;
        let first = gw.request("rel", false, false).await?;
        let second = gw.request("rel", false, false).await?;
        self.check(
            "local_rpm_limit",
            "rpm=1: second request goes to fallback without contacting primary",
            first.deployment() == "primary"
                && second.deployment() == "fallback"
                && second.attempts() == 1
                && p.hits() == 1,
            format!(
                "{} | {}",
                first.summary(self.secrets),
                second.summary(self.secrets)
            ),
        );
        drop(gw);
        let (url, _) = self.lab.route(
            &self.primary.base_url,
            "status",
            json!({"status":429,"headers":{"Retry-After":"7"}}),
        );
        let (furl, _) = self.lab.route(
            &self.fallback.base_url,
            "status",
            json!({"status":429,"headers":{"Retry-After":"3"}}),
        );
        let (pd, pc) = self.deployment("primary", self.primary, 0, &url, json!({}));
        let (fd, fc) = self.deployment("fallback", self.fallback, 1, &furl, json!({}));
        let gw = self.gateway(vec![pd, fd], vec![pc, fc], None).await?;
        let r = gw.request("rel", false, false).await?;
        let retry = r
            .headers
            .get("retry-after")
            .and_then(|s| s.parse::<usize>().ok());
        self.check(
            "all_rate_limited",
            "every deployment 429 -> gateway 429 with earliest Retry-After (3s)",
            r.status == 429 && retry.is_some_and(|n| (1..=3).contains(&n)),
            format!(
                "{} retry-after={}",
                r.summary(self.secrets),
                retry.unwrap_or(0)
            ),
        );
        drop(gw);
        self.shared_cooldown().await?;
        self.bad_key(self.primary, "bad_key_primary").await?;
        for (name, fault, options, extra, expect) in [
            (
                "server_503_failover",
                "status",
                json!({"status":503}),
                json!({}),
                "503 on primary -> fallback",
            ),
            (
                "timeout_hang_failover",
                "hang",
                json!({"seconds":30}),
                json!({"timeout_ms":deployment_timeout(3000)}),
                "primary accepts but never answers -> fallback after the 3s deployment timeout",
            ),
            (
                "outage_tcp_reset",
                "reset",
                json!({}),
                json!({}),
                "primary resets the connection -> fallback",
            ),
        ] {
            self.failover(name, expect, fault, options, extra, (false, false))
                .await?;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        for (name, base, extra, expect) in [
            (
                "outage_connection_refused",
                format!("http://127.0.0.1:{port}/v1"),
                json!({}),
                "nothing listens on primary -> fallback",
            ),
            (
                "outage_dns_failure",
                "https://llmux-outage.invalid/v1".into(),
                json!({}),
                "primary host does not resolve -> fallback",
            ),
            (
                "outage_blackhole",
                "https://10.255.255.1/v1".into(),
                json!({"timeout_ms":deployment_timeout(3000)}),
                "primary packets dropped -> fallback after the 3s deployment timeout",
            ),
        ] {
            let (furl, _) = self.lab.route(&self.fallback.base_url, "pass", json!({}));
            let (pd, pc) = self.deployment("primary", self.primary, 0, &base, extra);
            let (fd, fc) = self.deployment("fallback", self.fallback, 1, &furl, json!({}));
            let gw = self.gateway(vec![pd, fd], vec![pc, fc], None).await?;
            let r = gw.request("rel", false, false).await?;
            self.check(
                name,
                expect,
                r.status == 200
                    && r.deployment() == "fallback"
                    && r.attempts() == 2
                    && r.seconds < 30.0,
                r.summary(self.secrets),
            );
        }
        for (name, fault, options, extra, expect) in [
            (
                "stream_429_failover",
                "status",
                json!({"status":429,"headers":{"Retry-After":"5"}}),
                json!({}),
                "stream: 429 on primary -> fallback stream completes",
            ),
            (
                "stream_setup_timeout_failover",
                "hang",
                json!({"seconds":30}),
                json!({"timeout_ms":deployment_timeout(3000)}),
                "stream: primary never sends headers -> fallback after 3s",
            ),
            (
                "stream_close_before_first_event",
                "sse_then_close",
                json!({}),
                json!({}),
                "stream: primary sends 200 then drops before any event (uncommitted) -> fallback",
            ),
        ] {
            self.failover(name, expect, fault, options, extra, (true, false))
                .await?;
        }
        for (name,fault,extra,expect) in [("stream_midstream_cut","cut",json!({}),"stream: primary cut after 2 events -> committed stream ends with stream_broken error, no replay on fallback"),("stream_idle_stall","stall",json!({"timeout_ms":deployment_timeout(4000)}),"stream: primary stalls after 2 events -> stream_broken after the 4s idle timeout, no replay")] {let (gw,_,f)=self.pair(fault,json!({"cut_after_events":2,"seconds":30}),extra).await?;let r=gw.request("rel",true,false).await?;self.check(name,expect,r.status==200 && r.deployment()=="primary" && r.broken() && f.hits()==0 && if fault=="cut" {!r.done() && r.events()>=1}else{r.seconds<20.0},format!("{} fallback_hits={}",r.summary(self.secrets),f.hits()));}
        if self.fallback.provider == "anthropic" {
            self.skip(
                "responses_429_failover",
                "Responses API: 429 on primary -> fallback",
                "Anthropic fallback cannot serve the Responses API",
            );
        } else {
            self.failover(
                "responses_429_failover",
                "Responses API: 429 on primary -> fallback",
                "status",
                json!({"status":429,"headers":{"Retry-After":"5"}}),
                json!({}),
                (false, true),
            )
            .await?;
        }
        match crate::compare_gateways::read_key(&self.primary.key_ref) {
            Ok(key) if ["openai", "openai_compat"].contains(&self.primary.provider.as_str()) => {
                self.oauth(&key).await?
            }
            _ => self.skip(
                "oauth_refresh_on_401",
                "revoked access token -> one refresh -> retry succeeds on the same deployment",
                "primary is not OpenAI-compatible",
            ),
        };
        Ok(())
    }
    async fn shared_cooldown(&mut self) -> Result<()> {
        let (url, p) = self.lab.route(
            &self.primary.base_url,
            "status",
            json!({"status":429,"headers":{"Retry-After":"6"}}),
        );
        let (furl, _) = self.lab.route(&self.fallback.base_url, "pass", json!({}));
        let (pa, pc) = self.deployment("a-primary", self.primary, 0, &url, json!({}));
        let (fa, fc) = self.deployment("a-fallback", self.fallback, 1, &furl, json!({}));
        let mut pb = pa.clone();
        pb["id"] = json!("b-primary");
        let mut fb = fa.clone();
        fb["id"] = json!("b-fallback");
        let gw=self.gateway(vec![],vec![pc,fc],Some(json!([{"name":"rel-a","deployments":[pa,fa]},{"name":"rel-b","deployments":[pb,fb]}]))).await?;
        let first = gw.request("rel-a", false, false).await?;
        let second = gw.request("rel-b", false, false).await?;
        self.check("shared_route_cooldown","429 on model A's route -> model B, which lists the same provider/key/model, skips that route",first.deployment()=="a-fallback" && second.deployment()=="b-fallback" && second.attempts()==1 && p.hits()==1,format!("{} | {} primary_hits={}",first.summary(self.secrets),second.summary(self.secrets),p.hits()));
        Ok(())
    }
    async fn oauth(&mut self, key: &str) -> Result<()> {
        let directory = Directory::new()?;
        let path = directory.0.join("oauth.refresh");
        fs::write(
            &path,
            &self.lab.state.auth.lock().expect("auth lock").refresh,
        )?;
        let (url, p) = self
            .lab
            .route(&self.primary.base_url, "oauth", json!({"real_key":key}));
        let (furl, _) = self.lab.route(&self.fallback.base_url, "pass", json!({}));
        let (pd, _) = self.deployment(
            "primary",
            self.primary,
            0,
            &url,
            json!({"credential":"oauth-primary"}),
        );
        let (fd, fc) = self.deployment("fallback", self.fallback, 1, &furl, json!({}));
        let oc = json!({"id":"oauth-primary","kind":"oauth","provider":"openai_compat","token_url":self.lab.token_url(),"client_id":"llmux-reliability","refresh_token":format!("file:{}",path.display())});
        let gw = self.gateway(vec![pd, fd], vec![oc, fc], None).await?;
        let r = gw.request("rel", false, false).await?;
        self.check(
            "oauth_initial_token",
            "startup token is accepted by the real provider",
            r.status == 200 && r.deployment() == "primary",
            r.summary(self.secrets),
        );
        let grants = {
            let mut auth = self.lab.state.auth.lock().expect("auth lock");
            auth.tokens.clear();
            auth.grants
        };
        let r = gw.request("rel", false, false).await?;
        let refreshes = self.lab.state.auth.lock().expect("auth lock").grants - grants;
        self.check(
            "oauth_refresh_on_401",
            "revoked token -> one refresh -> same deployment succeeds (attempts 2)",
            r.status == 200 && r.deployment() == "primary" && r.attempts() == 2 && refreshes == 1,
            format!("{} refreshes={refreshes}", r.summary(self.secrets)),
        );
        {
            let mut auth = self.lab.state.auth.lock().expect("auth lock");
            auth.tokens.clear();
            auth.fail = true;
        }
        let r = gw.request("rel", false, false).await?;
        self.check(
            "oauth_refresh_failure_failover",
            "revoked token and refresh rejected -> fallback",
            r.status == 200 && r.deployment() == "fallback",
            r.summary(self.secrets),
        );
        let hits = p.hits();
        let r = gw.request("rel", false, false).await?;
        self.check(
            "oauth_disabled_after_failure",
            "credential with failed refresh is skipped next time",
            r.status == 200 && r.deployment() == "fallback" && p.hits() == hits,
            r.summary(self.secrets),
        );
        Ok(())
    }
}

pub async fn run(args: &[String]) -> Result<()> {
    let mut binary = "target/release/llmux-server".to_owned();
    let mut source = None;
    let mut primary_host = None;
    let mut fallback_host = None;
    let mut bad_hosts = "all".to_owned();
    let mut offline = false;
    let mut output = "live-reliability".to_owned();
    let mut flags = args.iter();
    while let Some(flag) = flags.next() {
        if flag == "--offline" {
            offline = true;
            continue;
        }
        let value = flags.next().ok_or("missing option value")?;
        match flag.as_str() {
            "--binary" => binary = value.clone(),
            "--config" => source = Some(value),
            "--primary" => primary_host = Some(value),
            "--fallback" => fallback_host = Some(value),
            "--bad-key-hosts" => bad_hosts = value.clone(),
            "--out" => output = value.clone(),
            _ => return Err("unknown option".into()),
        }
    }
    if !offline && source.is_none() {
        return Err("--config is required unless --offline".into());
    }
    let lab = Lab::start().await?;
    let other = Lab::start().await?;
    let directory = Directory::new()?;
    let mut secrets = Vec::new();
    let targets = if offline {
        let path = directory.0.join("offline.key");
        fs::write(&path, "offline-key")?;
        [lab.fake_url(), other.fake_url()]
            .into_iter()
            .enumerate()
            .map(|(i, base_url)| Target {
                name: format!("fake-{i}"),
                provider: "openai_compat".into(),
                base_url,
                key_ref: format!("file:{}", path.display()),
                upstream_model: "fake-model".into(),
            })
            .collect::<Vec<_>>()
    } else {
        targets_from_config(&parse_config(&fs::read_to_string(
            source.ok_or("missing config")?,
        )?)?)?
    };
    for target in &targets {
        client(&target.base_url)?;
        if let Ok(secret) = crate::compare_gateways::read_key(&target.key_ref) {
            secrets.push(secret);
        }
    }
    let mut probes = Vec::new();
    for target in &targets {
        let (url, route) = lab.route(&target.base_url, "pass", json!({}));
        let config = json!({"server":{"request_timeout_ms":60000},"routing":{"max_attempts":1},"credentials":[{"id":"c","kind":"api_key","provider":target.provider,"key":target.key_ref}],"models":[{"name":"rel","deployments":[{"id":"d","credential":"c","base_url":url,"upstream_model":target.upstream_model}]}]});
        let gw = Gateway::start(Path::new(&binary), config).await?;
        let r = gw.request("rel", false, false).await?;
        let observed = redact(&r.summary(&secrets), &secrets);
        println!("PROBE {:<60} {observed}", target.describe());
        probes.push(json!({"target":target.describe(),"healthy":r.status==200,"observed":observed,"upstream":route.statuses()}));
    }
    let primary = targets.iter().enumerate().position(|(i, t)| {
        probes[i]["healthy"] == true
            && primary_host.map_or_else(
                || ["openai", "openai_compat"].contains(&t.provider.as_str()),
                |h| t.host() == *h,
            )
    });
    let fallback = targets.iter().enumerate().position(|(i, t)| {
        Some(i) != primary
            && probes[i]["healthy"] == true
            && fallback_host.is_none_or(|h| t.host() == *h)
    });
    let mut report = json!({"binary":binary,"primary":primary.map(|i|targets[i].describe()),"fallback":fallback.map(|i|targets[i].describe()),"probes":probes,"results":[]});
    if let (Some(p), Some(f)) = (primary, fallback) {
        let mut suite = Suite {
            binary: Path::new(&binary),
            primary: &targets[p],
            fallback: &targets[f],
            lab: &lab,
            secrets: &secrets,
            results: vec![],
        };
        suite.run_all().await?;
        for (_, target) in targets.iter().enumerate().filter(|(i, t)| {
            *i != p
                && bad_hosts != "none"
                && (bad_hosts == "all" || bad_hosts.split(',').any(|h| h == t.host()))
        }) {
            suite.bad_key(target, "bad_key").await?;
        }
        report["results"] = json!(suite.results);
    } else {
        eprintln!(
            "need two healthy targets; only {} healthy",
            report["probes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["healthy"] == true)
                .count()
        );
    }
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&output)?;
    let path = Path::new(&output);
    private_report(
        &path.join("report.json"),
        &serde_json::to_string_pretty(&report)?,
    )?;
    private_report(&path.join("report.md"), &markdown(&report))?;
    let results = report["results"].as_array().ok_or("invalid results")?;
    let failed = results.iter().filter(|r| r["verdict"] == "FAIL").count();
    println!(
        "\n{} scenarios, {failed} failed; report in {output}",
        results.len()
    );
    if failed > 0 || results.is_empty() {
        return Err("reliability scenarios failed".into());
    }
    Ok(())
}
fn private_report(path: &Path, text: &str) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_summary_redacts_full_body_before_truncating() {
        let secret = "ZXCVBBOUNDARYKEY987654321".to_owned();
        let secrets = vec![secret.clone()];
        let reply = Reply {
            status: 502,
            headers: BTreeMap::new(),
            body: format!("{}{} trailing error", "a".repeat(154), secret),
            seconds: 0.0,
            stream: false,
        };
        let observed = redact(&reply.summary(&secrets), &secrets);
        assert!(
            !observed.contains(&secret[..6]),
            "secret fragment leaked: {observed}"
        );
        assert!(observed.contains("[REDAC"));
    }
    const CONFIG: &str = r#"
    [server]
    listen = "127.0.0.1:8090"
    client_keys = ["file:/tmp/client"]
    [[credentials]]
    id = "a"
    kind = "api_key"
    provider = "openai_compat"
    key = "file:/srv/a.key"
    [[credentials]]
    id = "o"
    kind = "oauth"
    provider = "openai_compat"
    token_url = "https://auth.example/token"
    client_id = "x"
    refresh_token = "file:/srv/o.key"
    [[models]]
    name = "m"
    [[models.deployments]]
    id = "m-0"
    credential = "a"
    base_url = "https://api.example.com/v1/"
    upstream_model = "up-1"
    priority = 0
    [[models.deployments]]
    id = "m-1"
    credential = "o"
    base_url = "https://oauth.example.com/v1"
    upstream_model = "up-2"
    [[models]]
    name = "n"
    [[models.deployments]]
    id = "n-0"
    credential = "a"
    base_url = "https://api.example.com/v1"
    upstream_model = "up-3"
    priority = 2
    "#;
    #[test]
    fn parses_and_renders_the_deployed_config_shape() {
        let config = parse_config(CONFIG).unwrap();
        assert_eq!(config["server"]["client_keys"], json!(["file:/tmp/client"]));
        assert_eq!(
            config["models"][0]["deployments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["m-0", "m-1"]
        );
        assert_eq!(config["models"][1]["deployments"][0]["priority"], 2);
        assert_eq!(
            parse_config(&render_config(&config).unwrap()).unwrap(),
            config
        );
    }
    #[test]
    fn one_api_key_target_per_base_url() {
        let targets = targets_from_config(&parse_config(CONFIG).unwrap()).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(
            (
                &*targets[0].base_url,
                &*targets[0].key_ref,
                &*targets[0].upstream_model
            ),
            ("https://api.example.com/v1", "file:/srv/a.key", "up-1")
        );
    }
    #[test]
    fn redacts_known_secrets_and_bearer_tokens() {
        assert_eq!(
            redact(
                "key sk-live-secret-value / Bearer abcdefghijkl / short",
                &["sk-live-secret-value".into(), "short".into()]
            ),
            "key [REDACTED] / Bearer [REDACTED] / short"
        );
    }
    #[tokio::test]
    async fn injected_status_is_returned_and_counted_without_forwarding() {
        let lab = Lab::start().await.unwrap();
        let (url, route) = lab.route(
            &lab.fake_url(),
            "status",
            json!({"status":429,"headers":{"Retry-After":"4"}}),
        );
        let reply = request(
            &format!("{url}/chat/completions"),
            "offline-key",
            json!({"model":"m","messages":[]}),
        )
        .await
        .unwrap();
        assert_eq!(reply.status, 429);
        assert_eq!(reply.headers["retry-after"], "4");
        assert_eq!(route.hits(), 1);
        assert_eq!(route.statuses(), vec![json!(429)]);
        assert_eq!(lab.fake_hits(), 0);
    }
    #[tokio::test]
    async fn pass_forwards_to_the_upstream_base_path() {
        let lab = Lab::start().await.unwrap();
        let (url, route) = lab.route(&lab.fake_url(), "pass", json!({}));
        let reply = request(
            &format!("{url}/chat/completions"),
            "offline-key",
            json!({"model":"m","messages":[]}),
        )
        .await
        .unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(route.statuses(), vec![json!(200)]);
        assert_eq!(lab.fake_hits(), 1);
        assert!(serde_json::from_str::<Value>(&reply.body)
            .unwrap()
            .get("choices")
            .is_some());
    }
    #[test]
    fn markdown_counts_verdicts() {
        let report = json!({"primary":"p","fallback":"f","binary":"b","probes":[],"results":[{"scenario":"a","expect":"x","verdict":"PASS","observed":"200","note":""},{"scenario":"b","expect":"y","verdict":"FAIL","observed":"502 | z","note":""}]});
        let text = markdown(&report);
        assert!(text.contains("## Scenarios (1 FAIL, 1 PASS)"));
        assert!(text.contains("| FAIL | b | y | 502 / z |"));
    }
    #[tokio::test]
    async fn reset_fault_sends_tcp_reset_rather_than_clean_eof() {
        use std::io::Write;
        let lab = Lab::start().await.unwrap();
        let (_, route) = lab.route(&lab.fake_url(), "reset", json!({}));
        let address = lab.address.clone();
        let error=tokio::task::spawn_blocking(move ||{let mut socket=std::net::TcpStream::connect(address).unwrap();socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();socket.write_all(b"POST /r/0/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}").unwrap();socket.read_to_end(&mut Vec::new()).unwrap_err()}).await.unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        assert_eq!(route.hits(), 1);
        assert_eq!(lab.fake_hits(), 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn offline_commands_write_private_reports_and_exercise_real_gateway_faults() {
        use std::os::unix::fs::PermissionsExt;
        let binaries = tokio::task::spawn_blocking(crate::process::build)
            .await
            .unwrap()
            .unwrap();
        let directory = Directory::new().unwrap();
        let binary = binaries["llmux-server"].to_string_lossy().into_owned();
        let output = directory.0.join("reliability");
        run(&[
            "--offline".into(),
            "--binary".into(),
            binary.clone(),
            "--out".into(),
            output.to_string_lossy().into_owned(),
        ])
        .await
        .unwrap();
        let report: Value =
            serde_json::from_str(&fs::read_to_string(output.join("report.json")).unwrap()).unwrap();
        assert_eq!(report["results"].as_array().unwrap().len(), 30);
        assert!(report["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["verdict"] == "PASS"));
        assert!(fs::read_to_string(output.join("report.md"))
            .unwrap()
            .contains("## Scenarios (30 PASS)"));
        assert_eq!(
            fs::metadata(output.join("report.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!fs::read_to_string(output.join("report.json"))
            .unwrap()
            .contains("offline-key"));
        let lab = Lab::start().await.unwrap();
        let key = directory.0.join("key");
        fs::write(&key, "offline-key").unwrap();
        for source in ["--model", "--models-file", "--coverage"] {
            let path = directory.0.join("models");
            let value = if source == "--model" {
                "fake-model".to_owned()
            } else {
                fs::write(
                    &path,
                    if source == "--models-file" {
                        "[\"fake-model\"]"
                    } else {
                        r#"{"rows":[{"name":"fake-model","calls":2,"present":true}]}"#
                    },
                )
                .unwrap();
                path.to_string_lossy().into_owned()
            };
            let output = directory.0.join(source.trim_start_matches('-'));
            let mut args = Vec::new();
            for name in ["a", "b"] {
                args.extend([
                    "--gateway".into(),
                    name.into(),
                    lab.fake_url(),
                    format!("file:{}", key.display()),
                ]);
            }
            args.extend([
                source.into(),
                value,
                "--responses".into(),
                "1".into(),
                "--output".into(),
                output.to_string_lossy().into_owned(),
            ]);
            crate::compare_gateways::run(&args).await.unwrap();
            let report: Value =
                serde_json::from_str(&fs::read_to_string(output.join("results.json")).unwrap())
                    .unwrap();
            assert_eq!(report["summary"]["requests"], 6);
            assert_eq!(report["summary"]["verdicts"]["match"], 3);
            assert!(!output.join("partial.json").exists());
            assert!(output.join("report.html").exists());
            assert_eq!(
                fs::metadata(output.join("results.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let source = directory.0.join("source.toml");
        let config = json!({"server":{},"credentials":[{"id":"c","kind":"api_key","provider":"openai_compat","key":format!("file:{}",key.display())}],"models":[{"name":"m","deployments":[{"id":"p","credential":"c","base_url":lab.fake_url(),"upstream_model":"fake-model"}]}]});
        fs::write(&source, render_config(&config).unwrap()).unwrap();
        let output = directory.0.join("insufficient");
        assert!(run(&[
            "--config".into(),
            source.to_string_lossy().into_owned(),
            "--binary".into(),
            binary,
            "--primary".into(),
            "127.0.0.1".into(),
            "--fallback".into(),
            "127.0.0.1".into(),
            "--bad-key-hosts".into(),
            "none".into(),
            "--out".into(),
            output.to_string_lossy().into_owned()
        ])
        .await
        .is_err());
        assert!(fs::read_to_string(output.join("report.md"))
            .unwrap()
            .contains("## Scenarios ()"));
        assert!(run(&[]).await.is_err());
        assert!(run(&["--unknown".into(), "x".into()]).await.is_err());
        assert!(parse_config("not TOML").is_err());
        crate::mock_upstream::run(&[
            "--requests".into(),
            "4".into(),
            "--concurrency".into(),
            "2".into(),
            "--workers".into(),
            "1".into(),
            "--fixed-rate-rounds".into(),
            "2".into(),
            "--upstream-delay-ms".into(),
            "1".into(),
        ])
        .await
        .unwrap();
    }
}
