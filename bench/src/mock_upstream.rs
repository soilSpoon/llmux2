use crate::{
    process::{self, Directory, Server},
    Result,
};
use http_body_util::{BodyExt, Full};
use hyper::{
    body::{Bytes, Incoming},
    Request,
};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fmt::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{Barrier, Semaphore},
    task::JoinSet,
};

type HttpClient = Client<HttpConnector, Full<Bytes>>;
type Timing = (Option<f64>, Option<f64>);
const TOKENS: usize = 8;
const RATE: f64 = 1000.0;

fn p99(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[(sorted.len() as f64 * 0.99).ceil() as usize - 1]
}
fn median(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    (sorted[(sorted.len() - 1) / 2] + sorted[sorted.len() / 2]) / 2.0
}
fn order(index: usize) -> [bool; 2] {
    if index.is_multiple_of(2) {
        [false, true]
    } else {
        [true, false]
    }
}
fn paired_rounds<T, F: FnMut(bool, bool) -> T>(rounds: usize, mut measure: F) -> Vec<(T, T)> {
    for gateway in order(0) {
        measure(gateway, false);
    }
    (0..rounds)
        .map(|index| {
            let [first, second] = order(index);
            let a = measure(first, true);
            let b = measure(second, true);
            if first {
                (b, a)
            } else {
                (a, b)
            }
        })
        .collect()
}
fn overhead_verdict(deltas: &[f64]) -> (f64, &'static str) {
    let delta = median(deltas);
    (delta, if delta < 1.0 { "PASS" } else { "FAIL" })
}
fn report_fixed_rate(pairs: &[(f64, f64)]) -> String {
    let mut text=String::from("\n### Fixed-rate JSON at 1,000 RPS\n\n| Round | Order | Direct p99 (ms) | Via gateway p99 (ms) | Gateway − direct (ms) |\n| ---: | --- | ---: | ---: | ---: |\n");
    let deltas: Vec<_> = pairs.iter().map(|(a, b)| b - a).collect();
    for (i, (direct, gateway)) in pairs.iter().enumerate() {
        let label = if order(i)[0] {
            "Gateway → direct"
        } else {
            "Direct → gateway"
        };
        writeln!(
            text,
            "| {} | {label} | {direct:.3} | {gateway:.3} | {:+.3} |",
            i + 1,
            gateway - direct
        )
        .expect("string write");
    }
    let (delta, verdict) = overhead_verdict(&deltas);
    let direct = median(&pairs.iter().map(|p| p.0).collect::<Vec<_>>());
    let gateway = median(&pairs.iter().map(|p| p.1).collect::<Vec<_>>());
    write!(text,"\nMedian direct p99: **{direct:.3} ms**; median via-gateway p99: **{gateway:.3} ms**.\n\nJSON gateway p99 overhead target: **{verdict}** (median paired delta {delta:+.3} ms; range {:+.3} to {:+.3} ms; target < 1 ms, advisory).\n",deltas.iter().copied().fold(f64::INFINITY,f64::min),deltas.iter().copied().fold(f64::NEG_INFINITY,f64::max)).expect("string write");
    text
}
fn arrival(start: f64, index: usize, rate: f64, now: f64) -> (f64, f64) {
    let scheduled = start + index as f64 / rate;
    (scheduled, (scheduled - now).max(0.0))
}

#[derive(Default)]
struct Completion {
    pending: Vec<u8>,
    tokens: usize,
    done: bool,
    first: Option<f64>,
}
impl Completion {
    fn feed(&mut self, bytes: &[u8], received: Option<f64>, start: Option<f64>) -> Result<()> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.windows(2).position(|w| w == b"\n\n") {
            let event: Vec<_> = self.pending.drain(..end + 2).collect();
            let Some(data) = event[..end].strip_prefix(b"data: ") else {
                continue;
            };
            if self.done {
                return Err("unexpected event after [DONE]".into());
            }
            if data == b"[DONE]" {
                self.done = true;
                continue;
            }
            let value: Value = serde_json::from_slice(data)?;
            if value.get("error").is_some() {
                return Err("mock stream error".into());
            }
            if value["choices"]
                .as_array()
                .ok_or("invalid choices")?
                .iter()
                .any(|choice| {
                    choice["delta"]["content"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty())
                })
            {
                self.tokens += 1;
                if self.first.is_none() {
                    self.first = received
                        .zip(start)
                        .map(|(received, start)| received - start);
                }
            }
        }
        Ok(())
    }
    fn finish(&self) -> Result<()> {
        if !self.pending.is_empty() || !self.done || self.tokens != TOKENS {
            Err(format!(
                "incomplete stream: {} tokens, [DONE]={}",
                self.tokens, self.done
            )
            .into())
        } else {
            Ok(())
        }
    }
}
fn validate_json(bytes: &[u8]) -> Result<()> {
    let value: Value = serde_json::from_slice(bytes)?;
    if value["choices"][0]["message"]["content"] != "x".repeat(TOKENS) {
        return Err("unexpected mock completion".into());
    }
    Ok(())
}
#[cfg(test)]
fn completion<F: FnMut() -> f64>(
    streaming: bool,
    chunks: &[&[u8]],
    scheduled: Option<f64>,
    timed: bool,
    mut clock: F,
) -> Result<Timing> {
    let start = if timed {
        scheduled.or_else(|| Some(clock()))
    } else {
        None
    };
    let mut parser = Completion::default();
    if streaming {
        for chunk in chunks {
            let received = timed.then(&mut clock);
            parser.feed(chunk, received, start)?;
        }
        parser.finish()?;
    } else {
        validate_json(&chunks.concat())?;
    }
    Ok((
        timed.then(|| clock() - start.expect("timed start")),
        parser.first,
    ))
}

#[derive(Clone)]
struct Chat {
    client: HttpClient,
    url: String,
    model: String,
    key: String,
    streaming: bool,
}
impl Chat {
    async fn start(&self) -> Result<Incoming> {
        let body=json!({"model":self.model,"messages":[{"role":"user","content":"Hello"}],"stream":self.streaming}).to_string();
        let request = Request::post(&self.url)
            .header("authorization", format!("Bearer {}", self.key))
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body)))?;
        let response = self.client.request(request).await?;
        let expected = if self.streaming {
            "text/event-stream"
        } else {
            "application/json"
        };
        if response.status() != 200
            || response
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.split(';').next())
                != Some(expected)
        {
            return Err("expected 200 with mock response content type".into());
        }
        Ok(response.into_body())
    }
    async fn request(&self, scheduled: Option<Instant>, timed: bool) -> Result<Timing> {
        tokio::time::timeout(
            Duration::from_secs(15),
            self.request_inner(scheduled, timed),
        )
        .await?
    }
    async fn request_inner(&self, scheduled: Option<Instant>, timed: bool) -> Result<Timing> {
        let start = if timed {
            scheduled.or_else(|| Some(Instant::now()))
        } else {
            None
        };
        let mut body = self.start().await?;
        let mut parser = Completion::default();
        if self.streaming {
            while let Some(frame) = body.frame().await {
                let frame = frame?;
                if let Some(bytes) = frame.data_ref() {
                    parser.feed(
                        bytes,
                        start.map(|s| s.elapsed().as_secs_f64()),
                        start.map(|_| 0.0),
                    )?;
                }
            }
            parser.finish()?;
        } else {
            validate_json(&body.collect().await?.to_bytes())?;
        }
        Ok((start.map(|s| s.elapsed().as_secs_f64()), parser.first))
    }
}

struct Target {
    chat: Chat,
    servers: Vec<Server>,
    _directory: Directory,
}
impl Target {
    fn new(
        binaries: &HashMap<String, PathBuf>,
        gateway: bool,
        hold: bool,
        args: &Args,
    ) -> Result<Self> {
        let mut flags = vec!["--delay-ms".into(), args.delay.to_string()];
        if hold {
            flags.push("--hold".into());
        }
        let upstream = Server::start(
            binaries
                .get("mock_load_upstream")
                .ok_or("missing mock binary")?,
            &flags,
            args.workers,
        )?;
        let directory = Directory::new()?;
        let mut address = upstream.address.clone();
        let mut servers = vec![upstream];
        if gateway {
            let config = directory.0.join("llmux.toml");
            std::fs::write(
                &config,
                format!(
                    r#"
[server]
listen = "127.0.0.1:0"
client_keys = ["env:LLMUX_BENCH_CLIENT_KEY"]
request_timeout_ms = 120000
[routing]
max_attempts = 1
[[credentials]]
id = "mock"
provider = "openai_compat"
kind = "api_key"
key = "env:LLMUX_BENCH_UPSTREAM_KEY"
[[models]]
name = "gateway-model"
capabilities = {{ stream = true }}
[[models.deployments]]
id = "mock"
credential = "mock"
base_url = "http://{address}/v1"
upstream_model = "backend-model"
priority = 0
"#
                ),
            )?;
            let server = Server::start(
                binaries
                    .get("llmux-server")
                    .ok_or("missing gateway binary")?,
                &[config.to_string_lossy().into_owned()],
                args.workers,
            )?;
            address = server.address.clone();
            servers.push(server);
        }
        let chat = Chat {
            client: Client::builder(TokioExecutor::new())
                .pool_max_idle_per_host(args.concurrency)
                .build_http(),
            url: format!("http://{address}/v1/chat/completions"),
            model: if gateway {
                "gateway-model"
            } else {
                "backend-model"
            }
            .into(),
            key: if gateway {
                "mock-gateway"
            } else {
                "mock-backend"
            }
            .into(),
            streaming: false,
        };
        Ok(Self {
            chat,
            servers,
            _directory: directory,
        })
    }
}

async fn warm(chat: &Chat, concurrency: usize) -> Result<()> {
    let mut tasks = JoinSet::new();
    for _ in 0..concurrency {
        let chat = chat.clone();
        tasks.spawn(async move {
            for _ in 0..4 {
                chat.request(None, false).await?;
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });
    }
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    Ok(())
}
async fn load(chat: &Chat, args: &Args, rate: Option<f64>, timed: bool) -> Result<(f64, f64, f64)> {
    warm(chat, args.concurrency).await?;
    let mut tasks = JoinSet::new();
    let start = Instant::now();
    if let Some(rate) = rate {
        let permits = Arc::new(Semaphore::new(args.concurrency));
        for i in 0..args.requests {
            let (scheduled, remaining) = arrival(0.0, i, rate, start.elapsed().as_secs_f64());
            if remaining > 0.0 {
                tokio::time::sleep_until((start + Duration::from_secs_f64(scheduled)).into()).await;
            }
            let chat = chat.clone();
            let permits = permits.clone();
            tasks.spawn(async move {
                let _permit = permits.acquire().await?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(vec![
                    chat.request(Some(start + Duration::from_secs_f64(scheduled)), timed)
                        .await?,
                ])
            });
        }
    } else {
        let barrier = Arc::new(Barrier::new(args.concurrency));
        for i in 0..args.concurrency {
            let chat = chat.clone();
            let barrier = barrier.clone();
            let count = args.requests / args.concurrency
                + usize::from(i < args.requests % args.concurrency);
            tasks.spawn(async move {
                barrier.wait().await;
                let mut results = Vec::new();
                for _ in 0..count {
                    results.push(chat.request(None, true).await?);
                }
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(results)
            });
        }
    }
    let mut latency = Vec::new();
    let mut first = Vec::new();
    while let Some(result) = tasks.join_next().await {
        for (a, b) in result?? {
            if let Some(a) = a {
                latency.push(a);
            }
            if let Some(b) = b {
                first.push(b);
            }
        }
    }
    Ok((
        if latency.is_empty() {
            0.0
        } else {
            p99(&latency) * 1000.0
        },
        if first.is_empty() {
            0.0
        } else {
            p99(&first) * 1000.0
        },
        args.requests as f64 / start.elapsed().as_secs_f64(),
    ))
}
fn pss_kib(servers: &[Server]) -> Result<f64> {
    servers
        .iter()
        .map(|server| {
            let text = std::fs::read_to_string(format!("/proc/{}/smaps_rollup", server.pid()))?;
            let line = text
                .lines()
                .find(|line| line.starts_with("Pss:"))
                .ok_or("missing PSS")?;
            Ok(line
                .split_whitespace()
                .nth(1)
                .ok_or("missing PSS value")?
                .parse::<f64>()?)
        })
        .sum()
}
async fn memory(target: &Target, concurrency: usize) -> Result<f64> {
    target.chat.request(None, true).await?;
    let baseline = pss_kib(&target.servers)?;
    let mut tasks = JoinSet::new();
    for _ in 0..concurrency {
        let mut chat = target.chat.clone();
        chat.streaming = true;
        tasks.spawn(async move {
            let mut body = chat.start().await?;
            let mut parser = Completion::default();
            while let Some(frame) = body.frame().await {
                if let Some(bytes) = frame?.data_ref() {
                    parser.feed(bytes, None, None)?;
                    if parser.tokens > 0 {
                        return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(body);
                    }
                }
            }
            Err("held stream ended before its first token".into())
        });
    }
    let mut bodies = Vec::new();
    while let Some(result) = tasks.join_next().await {
        bodies.push(result??);
    }
    let mut samples = Vec::new();
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(if cfg!(test) { 1 } else { 50 })).await;
        samples.push(pss_kib(&target.servers)?);
    }
    Ok((median(&samples) - baseline) / concurrency as f64)
}

struct Args {
    requests: usize,
    concurrency: usize,
    workers: usize,
    rounds: usize,
    delay: u64,
}
impl Args {
    fn parse(args: &[String]) -> Result<Self> {
        let mut config = Self {
            requests: 4096,
            concurrency: 64,
            workers: 2,
            rounds: 9,
            delay: 0,
        };
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or("missing argument value")?;
            match flag.as_str() {
                "--requests" => config.requests = value.parse()?,
                "--concurrency" => config.concurrency = value.parse()?,
                "--workers" => config.workers = value.parse()?,
                "--fixed-rate-rounds" => config.rounds = value.parse()?,
                "--upstream-delay-ms" => config.delay = value.parse()?,
                _ => return Err("unknown mock-upstream option".into()),
            }
        }
        if !Path::new("/proc/self/smaps_rollup").is_file() {
            return Err(
                "Linux with /proc/<pid>/smaps_rollup is required for PSS measurement".into(),
            );
        }
        if config.concurrency < 1 || config.concurrency > config.requests || config.workers < 1 {
            return Err("require 1 <= concurrency <= requests and workers >= 1".into());
        }
        if config.rounds < 2 {
            return Err("require at least 2 fixed-rate rounds".into());
        }
        if config.delay >= 15000 {
            return Err(
                "upstream delay must be nonnegative and below the 15-second client timeout".into(),
            );
        }
        Ok(config)
    }
}
pub async fn run(flags: &[String]) -> Result<()> {
    let args = Args::parse(flags)?;
    let binaries = process::build()?;
    let mut rows = Vec::new();
    for gateway in [false, true] {
        eprintln!(
            "Benchmarking {} (JSON, SSE, held streams)…",
            if gateway { "via gateway" } else { "direct" }
        );
        let mut row = Vec::new();
        for streaming in [false, true] {
            let mut target = Target::new(&binaries, gateway, false, &args)?;
            target.chat.streaming = streaming;
            let (latency, first, throughput) = load(&target.chat, &args, None, true).await?;
            row.extend([latency, throughput]);
            if streaming {
                row.push(first);
            }
        }
        row.push(
            memory(
                &Target::new(&binaries, gateway, true, &args)?,
                args.concurrency,
            )
            .await?,
        );
        rows.push(row);
    }
    let mut pairs = Vec::new();
    let mut schedule = Vec::new();
    paired_rounds(args.rounds, |gateway, timed| {
        schedule.push((gateway, timed))
    });
    for round in schedule.as_chunks::<2>().0 {
        let timed = round[0].1;
        let mut values = [0.0; 2];
        for &(gateway, _) in round {
            eprintln!(
                "{} {} (1,000 RPS JSON)…",
                if timed { "Benchmarking" } else { "Warming up" },
                if gateway { "via gateway" } else { "direct" }
            );
            let target = Target::new(&binaries, gateway, false, &args)?;
            values[usize::from(gateway)] = load(&target.chat, &args, Some(RATE), timed).await?.0;
        }
        if timed {
            pairs.push((values[0], values[1]));
        }
    }
    println!("{} requests per route and mode, {} workers/held streams, {} Tokio workers per server process, {} ms configured upstream delay, {} timed fixed-rate rounds after one untimed warm-up pair.\n",args.requests,args.concurrency,args.workers,args.delay,args.rounds);
    println!("| Metric | Direct | Via gateway | Gateway − direct |\n| --- | ---: | ---: | ---: |");
    for (i, label) in [
        "JSON p99 latency (ms)",
        "JSON throughput (req/s)",
        "SSE p99 completion latency (ms)",
        "SSE throughput (streams/s)",
        "SSE p99 time to first token (ms)",
        "Incremental PSS per held stream (KiB)",
    ]
    .iter()
    .enumerate()
    {
        println!(
            "| {label} | {:.3} | {:.3} | {:+.3} |",
            rows[0][i],
            rows[1][i],
            rows[1][i] - rows[0][i]
        );
    }
    print!("{}", report_fixed_rate(&pairs));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn p99_is_nearest_rank() {
        assert_eq!(
            p99(&(0..100).rev().map(f64::from).collect::<Vec<_>>()),
            98.0
        );
    }
    #[test]
    fn pairing_discards_untimed_warmup_and_alternates_timed_run_order() {
        let mut calls = Vec::new();
        let mut values = [100.0, 200.0, 2.0, 2.25, 4.5, 4.0, 1.0, 30.0].into_iter();
        let pairs = paired_rounds(3, |gateway, timed| {
            calls.push((gateway, timed));
            values.next().unwrap()
        });
        assert_eq!(
            calls,
            [
                (false, false),
                (true, false),
                (false, true),
                (true, true),
                (true, true),
                (false, true),
                (false, true),
                (true, true)
            ]
        );
        assert_eq!(pairs, [(2.0, 2.25), (4.0, 4.5), (1.0, 30.0)]);
        assert_eq!(
            overhead_verdict(&pairs.iter().map(|(a, b)| b - a).collect::<Vec<_>>()),
            (0.5, "PASS")
        );
    }
    #[test]
    fn limit_is_strict_and_uses_the_median() {
        assert_eq!(overhead_verdict(&[-0.25, 0.75, 1.25, 10.0]), (1.0, "FAIL"));
    }
    #[test]
    fn report_distinguishes_route_medians_from_paired_median() {
        let text = report_fixed_rate(&[(2.0, 2.25), (4.0, 4.5), (1.0, 30.0)]);
        for expected in [
            "Median direct p99: **2.000 ms**",
            "median via-gateway p99: **4.500 ms**",
            "**PASS** (median paired delta +0.500 ms",
        ] {
            assert!(text.contains(expected), "{text}");
        }
    }
    #[test]
    fn untimed_json_request_does_not_read_latency_clock() {
        assert_eq!(
            completion(
                false,
                &[br#"{"choices":[{"message":{"content":"xxxxxxxx"}}]}"#],
                None,
                false,
                || panic!("untimed clock read")
            )
            .unwrap(),
            (None, None)
        );
    }
    #[test]
    fn json_latency_can_include_time_since_scheduled_arrival() {
        assert_eq!(
            completion(
                false,
                &[br#"{"choices":[{"message":{"content":"xxxxxxxx"}}]}"#],
                Some(1.0),
                true,
                || 3.0
            )
            .unwrap(),
            (Some(2.0), None)
        );
    }
    #[test]
    fn arrivals_keep_fixed_schedule_when_dispatch_falls_behind() {
        let now = [0.0, 1.5, 2.5, 2.75];
        let results: Vec<_> = (0..4).map(|i| arrival(0.0, i, 1.0, now[i])).collect();
        assert_eq!(results, [(0.0, 0.0), (1.0, 0.0), (2.0, 0.0), (3.0, 0.25)]);
    }
    #[test]
    fn ttft_uses_complete_content_event_not_role_event() {
        let tail = format!(
            "\n{}",
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n".repeat(7)
        );
        let chunks:Vec<&[u8]>=vec![b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":",b"\"x\"}}]}\n",tail.as_bytes(),b"data: [DONE]\n\n"];
        let mut ticks = (0..6).map(f64::from);
        assert_eq!(
            completion(true, &chunks, None, true, || ticks.next().unwrap()).unwrap(),
            (Some(5.0), Some(3.0))
        );
    }
}
