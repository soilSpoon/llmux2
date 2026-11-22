use crate::{atomic_write, Result};
use serde_json::{json, Value};
use std::{
    env,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    time::{Duration, Instant},
};

const DEFAULT_LISTEN: &str = "127.0.0.1:8080";
const MAX_BODY: u64 = 1_048_576;
const PROMPT: &str = "Reply with the single word: ok";

#[derive(Debug)]
struct Options {
    listen: String,
    key_file: PathBuf,
    state_file: PathBuf,
    models: Option<Vec<String>>,
    per_run: usize,
    timeout: Duration,
    all_formats: bool,
}

fn usage() -> &'static str {
    "usage: llmux2-probe [-h] [--listen HOST:PORT] [--key-file PATH] [--state-file PATH] [--models a,b,c] [--per-run N] [--timeout SECS] [--all-formats]"
}
fn display_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}
fn parse(args: &[OsString]) -> std::result::Result<Option<Options>, String> {
    if args.first().is_some_and(|a| a == "-h" || a == "--help") {
        println!(
            "{}\n\nSynthetic probes for the running llmux2 gateway.\n\nProbes /healthz and /v1/models on every run, then exercises\n--per-run canonical models (default 1) through /v1/chat/completions,\nrotating through the sorted model list across runs. The rotation\ncursor lives in --state-file. Prints a JSON report; exit 0 when every\nprobe passes, 1 on any failure.",
            usage()
        );
        return Ok(None);
    }
    #[allow(deprecated)]
    let root = env::home_dir()
        .ok_or("Could not determine home directory")?
        .join("llmux2");
    let mut options = Options {
        listen: DEFAULT_LISTEN.into(),
        key_file: root.join("client.key"),
        state_file: root.join("probe-state.json"),
        models: None,
        per_run: 1,
        timeout: Duration::from_secs(30),
        all_formats: false,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].to_string_lossy();
        let (flag, mut inline) = arg
            .split_once('=')
            .map_or((arg.as_ref(), None), |(f, v)| (f, Some(v.to_string())));
        let mut take = |index: &mut usize, flag: &str| -> std::result::Result<String, String> {
            if let Some(value) = inline.take() {
                return Ok(value);
            }
            *index += 1;
            args.get(*index)
                .filter(|arg| !arg.as_bytes().starts_with(b"-"))
                .map(|arg| arg.to_string_lossy().into_owned())
                .ok_or_else(|| format!("argument {flag}: expected one argument"))
        };
        match flag {
            "--listen" => options.listen = take(&mut index, flag)?,
            "--key-file" => options.key_file = take(&mut index, flag)?.into(),
            "--state-file" => options.state_file = take(&mut index, flag)?.into(),
            "--models" => {
                let models: Vec<String> = take(&mut index, flag)?
                    .split(',')
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(str::to_string)
                    .collect();
                if models.is_empty() {
                    return Err("argument --models: expected at least one model".to_string());
                }
                options.models = Some(models)
            }
            "--per-run" => {
                options.per_run = take(&mut index, flag)?
                    .parse()
                    .map_err(|_| "argument --per-run: invalid value".to_string())?
            }
            "--timeout" => {
                let seconds: f64 = take(&mut index, flag)?
                    .parse()
                    .map_err(|_| "argument --timeout: invalid value".to_string())?;
                options.timeout = Duration::try_from_secs_f64(seconds)
                    .ok()
                    .filter(|timeout| !timeout.is_zero())
                    .ok_or_else(|| "argument --timeout: invalid value".to_string())?;
            }
            "--all-formats" => {
                if inline.is_some() {
                    return Err(format!(
                        "unrecognized arguments: {}",
                        display_args(&args[index..])
                    ));
                }
                options.all_formats = true;
            }
            _ => {
                return Err(format!(
                    "unrecognized arguments: {}",
                    display_args(&args[index..])
                ))
            }
        }
        index += 1;
    }
    Ok(Some(options))
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_body(
    reader: &mut impl BufRead,
    chunked: bool,
    length: Option<u64>,
) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut line = String::new();
            Read::by_ref(reader).take(65537).read_line(&mut line)?;
            let size = u64::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            if size == 0 {
                return Ok(body);
            }
            if body.len() as u64 + size > MAX_BODY {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "response body exceeds probe limit",
                ));
            }
            let start = body.len();
            body.resize(start + size as usize, 0);
            reader.read_exact(&mut body[start..])?;
            let mut crlf = [0; 2];
            reader.read_exact(&mut crlf)?;
            if crlf != *b"\r\n" {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Invalid chunk terminator",
                ));
            }
        }
    }
    match length {
        Some(length) => {
            if length > MAX_BODY {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "response body exceeds probe limit",
                ));
            }
            body.resize(length as usize, 0);
            reader.read_exact(&mut body)?;
        }
        None => {
            Read::by_ref(reader)
                .take(MAX_BODY + 1)
                .read_to_end(&mut body)?;
            if body.len() as u64 > MAX_BODY {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "response body exceeds probe limit",
                ));
            }
        }
    }
    Ok(body)
}

fn timed_out() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "probe request deadline exceeded",
    )
}

fn budget(deadline: Instant) -> std::io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(timed_out());
    }
    Ok(remaining)
}

fn connect(
    addresses: impl Iterator<Item = std::net::SocketAddr>,
    deadline: Instant,
) -> std::io::Result<TcpStream> {
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, budget(deadline)?.min(Duration::from_secs(10))) {
            Ok(stream) => return Ok(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(timed_out())
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no addresses")))
}

fn write_all(stream: &TcpStream, mut data: &[u8], deadline: Instant) -> std::io::Result<()> {
    let mut stream = stream;
    while !data.is_empty() {
        stream.set_write_timeout(Some(budget(deadline)?))?;
        let written = match stream.write(data) {
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(timed_out())
            }
            other => other?,
        };
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write probe request",
            ));
        }
        data = &data[written..];
    }
    Ok(())
}

struct DeadlineReader {
    reader: BufReader<TcpStream>,
    deadline: Instant,
}
impl Read for DeadlineReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(budget(self.deadline)?))?;
        match self.reader.read(buf) {
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                Err(timed_out())
            }
            other => other,
        }
    }
}
impl BufRead for DeadlineReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(budget(self.deadline)?))?;
        match self.reader.fill_buf() {
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                Err(timed_out())
            }
            other => other,
        }
    }
    fn consume(&mut self, amount: usize) {
        self.reader.consume(amount)
    }
}

fn request(
    listen: &str,
    method: &str,
    path: &str,
    key: Option<&str>,
    body: Option<&str>,
    timeout: Duration,
) -> Result<Response> {
    use std::net::ToSocketAddrs;
    let deadline = Instant::now() + timeout;
    let addresses: Vec<_> = listen.to_socket_addrs()?.collect();
    if addresses.is_empty() {
        return Err(format!("'{listen}' resolved to no addresses").into());
    }
    let stream = connect(addresses.into_iter(), deadline)?;
    let mut text = format!("{method} {path} HTTP/1.1\r\nHost: {listen}\r\nConnection: close\r\n");
    if let Some(key) = key {
        text += &format!("Authorization: Bearer {key}\r\n");
    }
    if let Some(body) = body {
        text += &format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
    }
    text += "\r\n";
    write_all(&stream, text.as_bytes(), deadline)?;
    if let Some(body) = body {
        write_all(&stream, body.as_bytes(), deadline)?;
    }
    let mut reader = DeadlineReader {
        reader: BufReader::new(stream),
        deadline,
    };
    let mut line = String::new();
    Read::by_ref(&mut reader).take(65537).read_line(&mut line)?;
    let mut status_line = line.split_whitespace();
    if !status_line.next().is_some_and(|s| s.starts_with("HTTP/")) {
        return Err("malformed HTTP response status line".into());
    }
    let status: u16 = status_line
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or("malformed HTTP response status line")?;
    let mut headers = Vec::new();
    let mut length = None;
    let mut chunked = false;
    for _ in 0..100 {
        line.clear();
        Read::by_ref(&mut reader).take(65537).read_line(&mut line)?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse::<u64>().ok();
            }
            if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.eq_ignore_ascii_case("chunked");
            }
            headers.push((name.trim().to_string(), value.to_string()));
        }
    }
    let body = read_body(&mut reader, chunked, length)?;
    Ok(Response {
        status,
        headers,
        body,
    })
}

fn record(name: &str, start: Instant, result: Result<Response>, ok: bool) -> Value {
    match result {
        Ok(response) => {
            let mut record = json!({
                "name": name,
                "ok": ok,
                "status": response.status,
                "latency_ms": start.elapsed().as_millis() as u64,
            });
            if let Some(deployment) = response
                .headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("x-llmux-deployment"))
                .map(|(_, v)| v.clone())
            {
                record["deployment"] = json!(deployment);
            }
            if !ok {
                let snippet: String = String::from_utf8_lossy(&response.body)
                    .chars()
                    .take(256)
                    .collect();
                record["error"] = json!(snippet.trim());
            }
            record
        }
        Err(error) => json!({
            "name": name,
            "ok": false,
            "status": Value::Null,
            "latency_ms": start.elapsed().as_millis() as u64,
            "error": error.to_string(),
        }),
    }
}

fn probe(
    options: &Options,
    method: &str,
    path: &str,
    key: Option<&str>,
    body: Option<&str>,
) -> Result<Response> {
    request(&options.listen, method, path, key, body, options.timeout)
}

fn usable(format: &str, body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    match format {
        "chat" => value["choices"].as_array().is_some_and(|c| !c.is_empty()),
        "responses" => value.get("output").is_some(),
        "anthropic" => value["content"].as_array().is_some_and(|c| !c.is_empty()),
        _ => false,
    }
}

fn entry(
    name: &str,
    start: Instant,
    result: Result<Response>,
    ok: bool,
    format: &str,
    skip_403: bool,
) -> Value {
    let mut entry = record(name, start, result, ok);
    if entry["status"] == 403 && skip_403 {
        entry["ok"] = json!(true);
        entry["skipped"] = json!("key is not permitted for this model");
        entry.as_object_mut().unwrap().remove("error");
    } else if entry["status"]
        .as_u64()
        .is_some_and(|s| (200..300).contains(&s))
        && !ok
    {
        entry["error"] = json!(format!("unusable {format} response body"));
    }
    entry
}

fn run(options: &Options) -> Result<bool> {
    let mut checks = Vec::new();
    let start = Instant::now();
    let health = probe(options, "GET", "/healthz", None, None).map(|response| {
        let ok = (200..300).contains(&response.status) && response.body == b"ok";
        (ok, response)
    });
    let healthy = health.as_ref().map(|(ok, _)| *ok).unwrap_or(false);
    checks.push(match health {
        Ok((ok, response)) => record("healthz", start, Ok(response), ok),
        Err(error) => record("healthz", start, Err(error), false),
    });
    // A listener that fails healthz is not llmux2; never send it the key.
    if !healthy {
        let ok = checks.iter().all(|c| c["ok"] == true);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": ok,
                "listen": options.listen,
                "checks": checks,
            }))?
        );
        return Ok(ok);
    }
    let key = fs::read_to_string(&options.key_file)
        .map_err(|e| format!("cannot read client key {}: {e}", options.key_file.display()))?;
    let key = key.trim();
    if key.is_empty() {
        return Err(format!("client key {} is empty", options.key_file.display()).into());
    }
    let start = Instant::now();
    let models = probe(options, "GET", "/v1/models", Some(key), None);
    let mut listed: Vec<String> = Vec::new();
    let mut catalog_valid = false;
    if let Ok(response) = &models {
        if let Ok(value) = serde_json::from_slice::<Value>(&response.body) {
            if let Some(data) = value["data"].as_array() {
                catalog_valid = true;
                listed = data
                    .iter()
                    .filter_map(|m| m["id"].as_str().map(str::to_string))
                    .collect();
                listed.sort_unstable();
                listed.dedup();
            }
        }
    }
    // A malformed catalog fails outright; a legitimately empty one only fails
    // when there is no pinned --models list left to probe.
    let ok = models
        .as_ref()
        .map(|r| {
            (200..300).contains(&r.status)
                && catalog_valid
                && (!listed.is_empty() || options.models.is_some())
        })
        .unwrap_or(false);
    checks.push(match models {
        Ok(response)
            if (200..300).contains(&response.status)
                && (!catalog_valid || (listed.is_empty() && options.models.is_none())) =>
        {
            record(
                "models",
                start,
                Ok(Response {
                    status: response.status,
                    headers: response.headers,
                    body: b"empty or malformed model catalog".to_vec(),
                }),
                false,
            )
        }
        other => record("models", start, other, ok),
    });
    let explicit = options.models.is_some();
    let mut pending_next = None;
    let selected: Vec<String> = match &options.models {
        Some(models) => models.clone(),
        None => {
            if listed.is_empty() || options.per_run == 0 {
                Vec::new()
            } else {
                let state: Value = fs::read_to_string(&options.state_file)
                    .ok()
                    .and_then(|text| serde_json::from_str(&text).ok())
                    .unwrap_or_else(|| json!({"next": 0}));
                let next = state["next"].as_u64().unwrap_or(0) as usize % listed.len();
                let count = options.per_run.min(listed.len());
                pending_next = Some((next + count) % listed.len());
                (0..count)
                    .map(|i| listed[(next + i) % listed.len()].clone())
                    .collect()
            }
        }
    };
    for model in &selected {
        let name = format!("chat:{model}");
        let start = Instant::now();
        let result = probe(
            options,
            "POST",
            "/v1/chat/completions",
            Some(key),
            Some(
                &json!({
                    "model": model,
                    "messages": [{"role": "user", "content": PROMPT}],
                    "max_tokens": 1,
                    "stream": false,
                })
                .to_string(),
            ),
        );
        let ok = result
            .as_ref()
            .map(|r| (200..300).contains(&r.status) && usable("chat", &r.body))
            .unwrap_or(false);
        checks.push(entry(&name, start, result, ok, "chat", !explicit));
    }
    if options.all_formats {
        let candidates: Vec<String> = options
            .models
            .clone()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| listed.clone());
        for (format, path, body) in [
            (
                "responses",
                "/v1/responses",
                json!({"input": PROMPT, "max_output_tokens": 16}),
            ),
            (
                "anthropic",
                "/v1/messages",
                json!({
                    "max_tokens": 1,
                    "messages": [{"role": "user", "content": PROMPT}],
                }),
            ),
        ] {
            // Walk candidates so a key-restricted model cannot leave a
            // permanent blind spot: skipped models yield to the next one.
            for model in &candidates {
                let name = format!("{format}:{model}");
                let start = Instant::now();
                let mut request_body = body.clone();
                request_body["model"] = json!(model);
                let result = probe(
                    options,
                    "POST",
                    path,
                    Some(key),
                    Some(&request_body.to_string()),
                );
                let ok = result
                    .as_ref()
                    .map(|r| (200..300).contains(&r.status) && usable(format, &r.body))
                    .unwrap_or(false);
                let skipped =
                    result.as_ref().map(|r| r.status == 403).unwrap_or(false) && !explicit;
                let entry = entry(&name, start, result, ok, format, !explicit);
                checks.push(entry);
                if !skipped {
                    break;
                }
            }
        }
    }
    // Advance the rotation cursor only after this run's probes ran, so a
    // crash mid-run retries the same models instead of skipping them.
    if let Some(next) = pending_next {
        atomic_write(
            &options.state_file,
            |file| file.write_all(json!({"next": next}).to_string().as_bytes()),
            0o600,
        )?;
    }
    let ok = checks.iter().all(|c| c["ok"] == true);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": ok,
            "listen": options.listen,
            "checks": checks,
        }))?
    );
    Ok(ok)
}

pub fn probe_entry() -> i32 {
    unsafe {
        libc::umask(0o077);
    }
    let args: Vec<_> = env::args_os().skip(1).collect();
    match parse(&args) {
        Ok(None) => 0,
        Ok(Some(options)) => match run(&options) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(error) => {
                eprintln!("llmux2: {error}");
                1
            }
        },
        Err(error) => {
            eprintln!("{}\nllmux2-probe: error: {error}", usage());
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn timeout_rejects_nonpositive_and_unrepresentable_values() {
        for value in ["NaN", "inf", "-1", "0"] {
            let args = [OsString::from("--timeout"), OsString::from(value)];
            assert!(parse(&args).is_err(), "--timeout {value}");
        }
        let args = [OsString::from("--timeout"), OsString::from("5")];
        let options = parse(&args).unwrap().unwrap();
        assert_eq!(options.timeout, Duration::from_secs(5));
    }

    #[test]
    fn connect_walks_addresses_until_one_accepts() {
        let dead = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let live_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let live = live_listener.local_addr().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = connect([dead, live].into_iter(), deadline).unwrap();
        assert_eq!(stream.peer_addr().unwrap(), live);
    }
}
