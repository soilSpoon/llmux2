use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Command, Output},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "llmux2-ops-probe-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn key(&self) -> PathBuf {
        let path = self.0.join("client.key");
        fs::write(&path, "probe-test-key\n").unwrap();
        path
    }
    fn state(&self) -> PathBuf {
        self.0.join("probe-state.json")
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_llmux2-probe"))
            .args(args)
            .env("HOME", &self.0)
            .output()
            .unwrap()
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    declared_length: Option<usize>,
    hang: bool,
    trickle: Option<Duration>,
}
impl Reply {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string(),
            declared_length: None,
            hang: false,
            trickle: None,
        }
    }
    fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![],
            body: body.into(),
            declared_length: None,
            hang: false,
            trickle: None,
        }
    }
    fn truncated(body: &str, declared_length: usize) -> Self {
        Self {
            status: 200,
            headers: vec![],
            body: body.into(),
            declared_length: Some(declared_length),
            hang: false,
            trickle: None,
        }
    }
    fn hang() -> Self {
        Self {
            status: 200,
            headers: vec![],
            body: String::new(),
            declared_length: None,
            hang: true,
            trickle: None,
        }
    }
    fn trickle(status: u16, body: &str, interval: Duration) -> Self {
        Self {
            status,
            headers: vec![],
            body: body.into(),
            declared_length: None,
            hang: false,
            trickle: Some(interval),
        }
    }
}
struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Value,
}
struct Gateway {
    listen: String,
    requests: Arc<Mutex<Vec<Request>>>,
}
impl Gateway {
    fn start(replies: Vec<Reply>) -> (Self, mpsc::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = listener.local_addr().unwrap().to_string();
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (stop, stopped) = mpsc::channel();
        let capture = requests.clone();
        thread::spawn(move || loop {
            listener.set_nonblocking(true).unwrap();
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let request = read_request(&mut stream);
                    capture.lock().unwrap().push(request);
                    let reply = replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| Reply::text(500, "no scripted reply"));
                    if reply.hang {
                        // Never respond; poll stop so the test can shut down.
                        for _ in 0..1200 {
                            if stopped.try_recv().is_ok() {
                                return;
                            }
                            thread::sleep(Duration::from_millis(25));
                        }
                        return;
                    }
                    let mut response = format!("HTTP/1.1 {}\r\n", status_line(reply.status));
                    let mut headers = reply.headers;
                    headers.push((
                        "content-length".into(),
                        reply
                            .declared_length
                            .unwrap_or(reply.body.len())
                            .to_string(),
                    ));
                    headers.push(("connection".into(), "close".into()));
                    for (name, value) in headers {
                        response += &format!("{name}: {value}\r\n");
                    }
                    response += "\r\n";
                    response += &reply.body;
                    if let Some(interval) = reply.trickle {
                        for byte in response.as_bytes() {
                            if stream.write_all(&[*byte]).is_err() {
                                break;
                            }
                            let _ = stream.flush();
                            thread::sleep(interval);
                        }
                    } else {
                        stream.write_all(response.as_bytes()).unwrap();
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if stopped.try_recv().is_ok() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                Err(_) => return,
            }
        });
        (Self { listen, requests }, stop)
    }
    fn paths(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| format!("{} {}", r.method, r.path))
            .collect()
    }
    fn chat_models(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .filter_map(|r| r.body["model"].as_str().map(str::to_string))
            .collect()
    }
    fn authorized(&self) -> bool {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path != "/healthz")
            .all(|r| r.authorization.as_deref() == Some("Bearer probe-test-key"))
    }
}
fn status_line(status: u16) -> &'static str {
    match status {
        200 => "200 OK",
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        403 => "403 Forbidden",
        404 => "404 Not Found",
        429 => "429 Too Many Requests",
        503 => "503 Service Unavailable",
        _ => "500 Internal Server Error",
    }
}
fn read_request(stream: &mut std::net::TcpStream) -> Request {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let (method, path) = (
        parts.next().unwrap().to_string(),
        parts.next().unwrap().to_string(),
    );
    let mut authorization = None;
    let mut length = 0;
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_string());
            }
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    Request {
        method,
        path,
        authorization,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    }
}
fn models_reply() -> Reply {
    Reply::json(
        200,
        json!({"object":"list","data":[
            {"id":"beta","object":"model"},
            {"id":"alpha","object":"model"},
            {"id":"gamma","object":"model"}
        ]}),
    )
}
fn listen_flag(home: &Home, gateway: &Gateway, extra: &[&str]) -> Output {
    let mut args = vec![
        "--listen".into(),
        gateway.listen.clone(),
        "--key-file".into(),
        home.key().to_string_lossy().into_owned(),
        "--state-file".into(),
        home.state().to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    let output = Command::new(env!("CARGO_BIN_EXE_llmux2-probe"))
        .args(&args)
        .env("HOME", &home.0)
        .output()
        .unwrap();
    output
}
fn summary(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout was not JSON: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn probe_help_and_argument_errors() {
    let home = Home::new();
    let output = home.run(&["--help"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("usage:"));
    let output = home.run(&["--bogus"]);
    assert_eq!(output.status.code(), Some(2));
    let output = home.run(&["--timeout", "abc"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn nonpositive_or_unrepresentable_timeout_is_an_argument_error() {
    let home = Home::new();
    for args in [
        vec!["--timeout", "NaN"],
        vec!["--timeout", "inf"],
        vec!["--timeout=-1"],
        vec!["--timeout", "0"],
    ] {
        let output = home.run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
    let output = home.run(&["--timeout", "5"]);
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn all_probe_formats_send_provider_compatible_token_limits() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(200, json!({"choices":[{}]})),
        Reply::json(200, json!({"output":[]})),
        Reply::json(200, json!({"content":[{"type":"text"}]})),
    ]);
    let output = listen_flag(&home, &gateway, &["--all-formats"]);
    stop.send(()).unwrap();
    assert_eq!(output.status.code(), Some(0));
    let requests = gateway.requests.lock().unwrap();
    let limits: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "POST")
        .map(|request| {
            let field = if request.path == "/v1/responses" {
                "max_output_tokens"
            } else {
                "max_tokens"
            };
            (request.path.as_str(), request.body[field].as_u64())
        })
        .collect();
    assert_eq!(limits.len(), 3);
    assert!(
        limits
            .iter()
            .all(|(_, limit)| limit.is_some_and(|n| n >= 16)),
        "probe token limits must be at least 16: {limits:?}"
    );
}

#[test]
fn healthy_gateway_probes_each_route_and_reports_json() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(200, json!({"choices":[{"message":{"content":"ok"}}]})),
    ]);
    let output = listen_flag(&home, &gateway, &[]);
    assert_eq!(output.status.code(), Some(0));
    let report = summary(&output);
    assert_eq!(report["ok"], true);
    let names: Vec<_> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["healthz", "models", "chat:alpha"]);
    assert!(gateway.authorized());
    assert_eq!(
        gateway.paths(),
        [
            "GET /healthz",
            "GET /v1/models",
            "POST /v1/chat/completions"
        ]
    );
    stop.send(()).unwrap();
}

#[test]
fn rotation_walks_the_sorted_model_list_across_runs() {
    let home = Home::new();
    for (expected, code) in [("alpha", 0), ("beta", 0), ("gamma", 0), ("alpha", 0)] {
        let (gateway, stop) = Gateway::start(vec![
            Reply::text(200, "ok"),
            models_reply(),
            Reply::json(200, json!({"choices":[{}]})),
        ]);
        let output = listen_flag(&home, &gateway, &[]);
        assert_eq!(output.status.code(), Some(code));
        assert_eq!(gateway.chat_models(), [expected]);
        stop.send(()).unwrap();
    }
    let state: Value = serde_json::from_str(&fs::read_to_string(home.state()).unwrap()).unwrap();
    assert_eq!(state["next"], 1);
}

#[test]
fn failing_model_probe_exits_one_and_reports_status() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(429, json!({"error":{"message":"quota"}})),
    ]);
    let output = listen_flag(&home, &gateway, &[]);
    assert_eq!(output.status.code(), Some(1));
    let report = summary(&output);
    assert_eq!(report["ok"], false);
    let chat = &report["checks"][2];
    assert_eq!(chat["status"], 429);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("probe-test-key"));
    stop.send(()).unwrap();
}

#[test]
fn unreachable_gateway_reports_error_and_exits_one() {
    let home = Home::new();
    let dead = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().to_string()
    };
    let output = listen_flag(
        &home,
        &Gateway {
            listen: dead,
            requests: Arc::new(Mutex::new(vec![])),
        },
        &[],
    );
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn explicit_models_probe_each_listed_model() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(200, json!({"choices":[{}]})),
        Reply::json(200, json!({"choices":[{}]})),
    ]);
    let output = listen_flag(&home, &gateway, &["--models", "gamma,alpha"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(gateway.chat_models(), ["gamma", "alpha"]);
    stop.send(()).unwrap();
}

#[test]
fn all_formats_adds_responses_and_anthropic_probes() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(200, json!({"choices":[{}]})),
        Reply::json(200, json!({"output":[]})),
        Reply::json(200, json!({"content":[{"type":"text"}]})),
    ]);
    let output = listen_flag(&home, &gateway, &["--all-formats"]);
    assert_eq!(output.status.code(), Some(0));
    let report = summary(&output);
    let names: Vec<_> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        [
            "healthz",
            "models",
            "chat:alpha",
            "responses:alpha",
            "anthropic:alpha"
        ]
    );
    assert_eq!(
        gateway.paths(),
        [
            "GET /healthz",
            "GET /v1/models",
            "POST /v1/chat/completions",
            "POST /v1/responses",
            "POST /v1/messages"
        ]
    );
    stop.send(()).unwrap();
}

#[test]
fn healthz_failure_never_sends_the_key() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![Reply::text(503, "no")]);
    let output = listen_flag(&home, &gateway, &[]);
    assert_eq!(output.status.code(), Some(1));
    let report = summary(&output);
    assert_eq!(report["ok"], false);
    assert_eq!(gateway.paths(), ["GET /healthz"]);
    stop.send(()).unwrap();
}

#[test]
fn forbidden_model_is_skipped_not_failed() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(403, json!({"error":{"message":"model_not_allowed"}})),
    ]);
    let output = listen_flag(&home, &gateway, &[]);
    assert_eq!(output.status.code(), Some(0));
    let report = summary(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(
        report["checks"][2]["skipped"],
        "key is not permitted for this model"
    );
    stop.send(()).unwrap();
}

#[test]
fn empty_model_catalog_fails_the_models_check() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        Reply::json(200, json!({"object":"list","data":[]})),
    ]);
    let output = listen_flag(&home, &gateway, &[]);
    assert_eq!(output.status.code(), Some(1));
    let report = summary(&output);
    assert_eq!(report["checks"][1]["ok"], false);
    stop.send(()).unwrap();
}

#[test]
fn unusable_chat_body_fails_the_probe() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(200, json!({"unexpected": true})),
    ]);
    let output = listen_flag(&home, &gateway, &[]);
    assert_eq!(output.status.code(), Some(1));
    let report = summary(&output);
    assert_eq!(report["checks"][2]["error"], "unusable chat response body");
    stop.send(()).unwrap();
}

#[test]
fn pinned_forbidden_model_fails() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(403, json!({"error":{"message":"model_not_allowed"}})),
    ]);
    let output = listen_flag(&home, &gateway, &["--models", "alpha"]);
    assert_eq!(output.status.code(), Some(1));
    let report = summary(&output);
    assert_eq!(report["checks"][2]["ok"], false);
    assert!(report["checks"][2]["skipped"].is_null());
    stop.send(()).unwrap();
}

#[test]
fn format_probes_walk_past_key_restricted_models() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(403, json!({"error":{"message":"model_not_allowed"}})),
        Reply::json(403, json!({"error":{"message":"model_not_allowed"}})),
        Reply::json(200, json!({"output":[]})),
        Reply::json(403, json!({"error":{"message":"model_not_allowed"}})),
        Reply::json(200, json!({"content":[{"type":"text"}]})),
    ]);
    let output = listen_flag(&home, &gateway, &["--all-formats"]);
    assert_eq!(output.status.code(), Some(0));
    let report = summary(&output);
    assert_eq!(report["ok"], true);
    let names: Vec<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "healthz",
            "models",
            "chat:alpha",
            "responses:alpha",
            "responses:beta",
            "anthropic:alpha",
            "anthropic:beta"
        ]
    );
    assert_eq!(
        report["checks"][3]["skipped"],
        "key is not permitted for this model"
    );
    assert_eq!(report["checks"][4]["ok"], true);
    stop.send(()).unwrap();
}

#[test]
fn empty_catalog_with_pinned_models_still_verifies() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        Reply::json(200, json!({"object":"list","data":[]})),
        Reply::json(200, json!({"choices":[{}]})),
    ]);
    let output = listen_flag(&home, &gateway, &["--models", "pinned"]);
    assert_eq!(output.status.code(), Some(0));
    let report = summary(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(report["checks"][1]["ok"], true);
    assert_eq!(report["checks"][2]["name"], "chat:pinned");
    stop.send(()).unwrap();
}

#[test]
fn empty_models_pin_is_an_argument_error() {
    let home = Home::new();
    let output = home.run(&["--models", ""]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--models"));
}

#[test]
fn truncated_body_fails_the_health_check() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![Reply::truncated("ok", 100)]);
    let output = listen_flag(&home, &gateway, &[]);
    assert_eq!(output.status.code(), Some(1));
    let report = summary(&output);
    assert_eq!(report["checks"][0]["ok"], false);
    assert!(report["checks"][0]["error"]
        .as_str()
        .unwrap()
        .contains("failed to fill whole buffer"));
    stop.send(()).unwrap();
}

#[test]
fn trickling_response_still_fails_at_the_request_deadline() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![
        Reply::trickle(200, "ok", Duration::from_millis(50)),
        models_reply(),
        Reply::json(200, json!({"choices":[{}]})),
    ]);
    let output = listen_flag(&home, &gateway, &["--timeout", "1"]);
    assert_eq!(output.status.code(), Some(1));
    let report = summary(&output);
    assert_eq!(report["checks"][0]["ok"], false);
    assert!(report["checks"][0]["error"]
        .as_str()
        .unwrap()
        .contains("deadline"));
    stop.send(()).unwrap();
}

#[test]
fn killed_probe_does_not_advance_the_rotation_cursor() {
    let home = Home::new();
    let (gateway, stop) =
        Gateway::start(vec![Reply::text(200, "ok"), models_reply(), Reply::hang()]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_llmux2-probe"))
        .args([
            "--listen".into(),
            gateway.listen.clone(),
            "--key-file".into(),
            home.key().to_string_lossy().into_owned(),
            "--state-file".into(),
            home.state().to_string_lossy().into_owned(),
            "--timeout".into(),
            "300".into(),
        ])
        .env("HOME", &home.0)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while gateway.paths().len() < 3 {
        assert!(
            Instant::now() < deadline,
            "probe never reached the chat probe"
        );
        thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    let _ = child.wait().unwrap();
    let state = fs::read_to_string(home.state())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    assert!(
        state.as_ref().is_none_or(|s| s["next"].as_u64() == Some(0)),
        "rotation cursor advanced before the probes completed"
    );
    stop.send(()).unwrap();
}

#[test]
fn consecutive_404s_mark_a_model_dead_after_the_threshold() {
    let home = Home::new();
    let dead = || Reply::json(404, json!({"error":{"code":"model_not_found"}}));
    let ok = || Reply::json(200, json!({"choices":[{"message":{"content":"ok"}}]}));
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        dead(), // run 1: alpha 404
        Reply::text(200, "ok"),
        models_reply(),
        ok(), // run 2: beta ok
        Reply::text(200, "ok"),
        models_reply(),
        ok(), // run 3: gamma ok
        Reply::text(200, "ok"),
        models_reply(),
        dead(), // run 4: alpha 404 again
    ]);
    for (model, code, dead_count) in [
        ("alpha", 1, 0),
        ("beta", 0, 0),
        ("gamma", 0, 0),
        ("alpha", 1, 1),
    ] {
        let output = listen_flag(&home, &gateway, &[]);
        assert_eq!(output.status.code(), Some(code), "{model}");
        assert_eq!(gateway.chat_models().last().unwrap(), model);
        let report = summary(&output);
        assert_eq!(report["dead"].as_array().unwrap().len(), dead_count);
        if dead_count == 1 {
            assert_eq!(report["dead"], json!(["alpha"]));
        }
    }
    stop.send(()).unwrap();
}

#[test]
fn a_success_clears_the_failure_streak() {
    let home = Home::new();
    let dead = || Reply::json(404, json!({"error":{"code":"model_not_found"}}));
    let ok = || Reply::json(200, json!({"choices":[{"message":{"content":"ok"}}]}));
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        dead(),
        Reply::text(200, "ok"),
        models_reply(),
        dead(),
        Reply::text(200, "ok"),
        models_reply(),
        ok(),
    ]);
    for (code, expected) in [(1, 0), (1, 1), (0, 0)] {
        let output = listen_flag(&home, &gateway, &["--models", "alpha"]);
        assert_eq!(output.status.code(), Some(code));
        let report = summary(&output);
        let dead_list: Vec<_> = report["dead"]
            .as_array()
            .unwrap()
            .iter()
            .map(Value::as_str)
            .collect();
        assert_eq!(dead_list.len(), expected, "{:?}", report["dead"]);
        if expected == 1 {
            assert_eq!(dead_list, [Some("alpha")]);
        }
    }
    stop.send(()).unwrap();
}

#[test]
fn transient_statuses_never_count_as_dead() {
    let home = Home::new();
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(429, json!({"error":{"message":"cooldown"}})),
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(502, json!({"error":{"message":"upstream_error"}})),
    ]);
    for _ in 0..2 {
        let output = listen_flag(&home, &gateway, &["--models", "alpha", "--dead-after", "1"]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(summary(&output)["dead"], json!([]));
    }
    stop.send(()).unwrap();
}

#[test]
fn a_non_404_failure_resets_the_streak() {
    let home = Home::new();
    let dead = || Reply::json(404, json!({"error":{"code":"model_not_found"}}));
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(429, json!({"error":{"message":"cooldown"}})),
        Reply::text(200, "ok"),
        models_reply(),
        dead(),
        Reply::text(200, "ok"),
        models_reply(),
        dead(),
    ]);
    // 429 then 404 is one consecutive 404, not two: the streak must not fire.
    for expected in [0, 0, 1] {
        let output = listen_flag(&home, &gateway, &["--models", "alpha"]);
        assert_eq!(summary(&output)["dead"].as_array().unwrap().len(), expected);
    }
    stop.send(()).unwrap();
}

#[test]
fn sweep_probes_every_listed_model_without_moving_the_cursor() {
    let home = Home::new();
    let ok = || Reply::json(200, json!({"choices":[{"message":{"content":"ok"}}]}));
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        ok(),
        ok(),
        ok(),
        Reply::text(200, "ok"),
        models_reply(),
        ok(),
    ]);
    let output = listen_flag(&home, &gateway, &["--sweep"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(gateway.chat_models(), ["alpha", "beta", "gamma"]);
    let state: Value = serde_json::from_str(&fs::read_to_string(home.state()).unwrap()).unwrap();
    assert_eq!(state["next"], json!(0));
    // A routine run afterwards still rotates from the untouched cursor.
    let output = listen_flag(&home, &gateway, &[]);
    assert_eq!(gateway.chat_models().last().unwrap(), "alpha");
    assert_eq!(summary(&output)["dead"], json!([]));
    stop.send(()).unwrap();
}

#[test]
fn sweep_rejects_a_pinned_model_list() {
    let home = Home::new();
    let output = home.run(&["--sweep", "--models", "alpha"]);
    assert!(output.status.code().is_some_and(|code| code > 0));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("--sweep cannot be combined with --models"));
}

#[test]
fn sweep_accumulates_streaks_toward_the_dead_list() {
    let home = Home::new();
    let dead = || Reply::json(404, json!({"error":{"code":"model_not_found"}}));
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        dead(),
        dead(),
        dead(),
    ]);
    let output = listen_flag(&home, &gateway, &["--sweep", "--dead-after", "1"]);
    assert_eq!(summary(&output)["dead"], json!(["alpha", "beta", "gamma"]));
    stop.send(()).unwrap();
}

#[test]
fn dead_file_writes_the_dead_model_list() {
    let home = Home::new();
    let dead_file = home.0.join("dead.json");
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(404, json!({"error":{"code":"model_not_found"}})),
    ]);
    let output = listen_flag(
        &home,
        &gateway,
        &[
            "--models",
            "alpha",
            "--dead-after",
            "1",
            "--dead-file",
            dead_file.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(summary(&output)["dead"], json!(["alpha"]));
    let written: Value = serde_json::from_str(&fs::read_to_string(dead_file).unwrap()).unwrap();
    assert_eq!(written, json!(["alpha"]));
    stop.send(()).unwrap();
}

#[test]
fn dead_after_rejects_zero_and_nonnumeric_values() {
    let home = Home::new();
    for args in [
        vec!["--dead-after", "0"],
        vec!["--dead-after", "abc"],
        vec!["--dead-after=-1"],
    ] {
        let output = home.run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn unsavable_rotation_cursor_still_reports_probe_results() {
    let home = Home::new();
    let blocker = home.0.join("blocker");
    fs::write(&blocker, "x").unwrap();
    let (gateway, stop) = Gateway::start(vec![
        Reply::text(200, "ok"),
        models_reply(),
        Reply::json(200, json!({"choices":[{}]})),
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_llmux2-probe"))
        .args([
            "--listen",
            &gateway.listen,
            "--key-file",
            &home.key().to_string_lossy(),
            "--state-file",
            &blocker.join("state.json").to_string_lossy(),
        ])
        .env("HOME", &home.0)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report = summary(&output);
    let checks = report["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 4);
    assert_eq!(checks[3]["name"], "rotation-state");
    assert_eq!(checks[3]["ok"], false);
    assert_eq!(checks[2]["ok"], true);
    stop.send(()).unwrap();
}
