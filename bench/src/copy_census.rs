//! In-process copy/allocation census for the routed request path.
//!
//! The numbers are gross allocator requests measured around otherwise idle
//! loopback workloads. They are not memcpy counts and include Tokio, Hyper,
//! axum, serde, and the test fixtures; the direct leg is subtracted so the
//! remaining delta is the gateway-attributable steady-state cost.

use crate::{alloc_counter, Result};
use bytes::Bytes;
use http::{header, HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use llmux_config::RuntimeConfig;
use llmux_providers::{OpenAiCompat, RequestMetadata};
use llmux_testkit::{mock_load_upstream, TestServer};
use llmux_types::{
    provider, AuthMaterial, Billing, Deployment, DeploymentIdx, GatewayRequest, InboundFormat,
    ProviderAdapter, ProviderKind, QuotaSpec, RequestId,
};
use std::{fmt::Write, sync::Arc, time::Duration};

type HttpClient = Client<HttpConnector, Full<Bytes>>;

const GATEWAY_MODEL: &str = "gateway-model";
const BACKEND_MODEL: &str = "backend-model";
const CLIENT_KEY: &str = "census-client";
const PAIRS: usize = 3;

#[derive(Clone, Copy)]
struct EndToEndCase {
    name: &'static str,
    target_bytes: usize,
    stream: bool,
    memory_bytes: usize,
    retry: bool,
    requests: usize,
}

#[derive(Clone, Copy, Debug)]
struct Batch {
    allocations: f64,
    bytes: f64,
}

#[derive(Clone, Copy, Debug)]
struct PairResult {
    direct: Batch,
    gateway: Batch,
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    (sorted[(sorted.len() - 1) / 2] + sorted[sorted.len() / 2]) / 2.0
}

fn median_batch(values: &[Batch]) -> Batch {
    Batch {
        allocations: median(
            &values
                .iter()
                .map(|batch| batch.allocations)
                .collect::<Vec<_>>(),
        ),
        bytes: median(&values.iter().map(|batch| batch.bytes).collect::<Vec<_>>()),
    }
}

fn padded_json(prefix: &str, suffix: &str, target: usize) -> Bytes {
    let fixed = prefix.len() + suffix.len();
    let padding = target.saturating_sub(fixed);
    Bytes::from(format!("{prefix}{}{suffix}", "x".repeat(padding)))
}

fn chat_body(model: &str, stream: bool, target: usize) -> Bytes {
    padded_json(
        &format!(
            "{{\"model\":\"{model}\",\"messages\":[{{\"role\":\"user\",\"content\":\"hello\"}}],\"stream\":{stream},\"pad\":\""
        ),
        "\"}",
        target,
    )
}

fn responses_body(model: &str, target: usize) -> Bytes {
    padded_json(
        &format!("{{\"model\":\"{model}\",\"input\":\""),
        "\",\"stream\":false}",
        target,
    )
}

fn anthropic_body(model: &str, target: usize) -> Bytes {
    padded_json(
        &format!(
            "{{\"model\":\"{model}\",\"max_tokens\":32,\"messages\":[{{\"role\":\"user\",\"content\":\""
        ),
        "\"}]}",
        target,
    )
}

fn client() -> HttpClient {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn send(
    client: &HttpClient,
    uri: &str,
    bearer: &str,
    body: &Bytes,
) -> Result<(StatusCode, Bytes, Option<usize>)> {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .body(Full::new(body.clone()))?;
    let response = client.request(request).await?;
    let status = response.status();
    let attempts = response
        .headers()
        .get("x-llmux-attempts")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    let body = response.into_body().collect().await?.to_bytes();
    Ok((status, body, attempts))
}

async fn measure_endpoint(
    client: &HttpClient,
    uri: &str,
    bearer: &str,
    body: &Bytes,
    requests: usize,
) -> Result<(Batch, usize, Option<usize>)> {
    let start = alloc_counter::snapshot();
    let mut response_bytes = 0;
    let mut attempts_seen = None;
    for _ in 0..requests {
        let (status, response, attempts) = send(client, uri, bearer, body).await?;
        if !status.is_success() {
            return Err(format!(
                "copy-census request failed: {status} {}",
                String::from_utf8_lossy(&response)
            )
            .into());
        }
        response_bytes = response_bytes.max(response.len());
        attempts_seen = attempts_seen.or(attempts);
    }
    tokio::time::sleep(Duration::from_millis(2)).await;
    let delta = alloc_counter::delta_since(start);
    Ok((
        Batch {
            allocations: delta.count as f64 / requests as f64,
            bytes: delta.bytes as f64 / requests as f64,
        },
        response_bytes,
        attempts_seen,
    ))
}

fn gateway_config(
    upstream_url: &str,
    spool_dir: &std::path::Path,
    memory_bytes: usize,
    retry: bool,
) -> String {
    let second = if retry {
        format!(
            r#"
[[models.deployments]]
id = "good"
credential = "upstream"
base_url = "{upstream_url}/v1"
upstream_model = "{BACKEND_MODEL}"
priority = 10
"#
        )
    } else {
        String::new()
    };
    let (first_url, first_model, first_priority) = if retry {
        ("http://127.0.0.1:9".to_string(), BACKEND_MODEL, 0)
    } else {
        (upstream_url.to_string(), BACKEND_MODEL, 0)
    };
    format!(
        r#"
[server]
client_keys = ["env:LLMUX_BENCH_CENSUS_CLIENT_KEY"]
request_timeout_ms = 4000
expose_routing_headers = true
max_request_bytes = 1048576
request_body_memory_bytes = {memory_bytes}
request_body_spool_dir = {spool_dir:?}

[routing]
max_attempts = {max_attempts}
fallback_on = ["rate_limited", "quota_exhausted"]

[[credentials]]
id = "upstream"
kind = "api_key"
provider = "openai_compat"
key = "env:LLMUX_BENCH_CENSUS_UPSTREAM_KEY"

[[models]]
name = "{GATEWAY_MODEL}"
capabilities = {{ stream = true, max_context = 100000 }}

[[models.deployments]]
id = "first"
credential = "upstream"
base_url = "{first_url}/v1"
upstream_model = "{first_model}"
priority = {first_priority}
{second}
"#,
        max_attempts = if retry { 2 } else { 1 },
    )
}

async fn end_to_end_case(case: EndToEndCase, pairs: usize) -> Result<String> {
    std::env::set_var("LLMUX_BENCH_CENSUS_CLIENT_KEY", CLIENT_KEY);
    std::env::set_var("LLMUX_BENCH_CENSUS_UPSTREAM_KEY", "mock-backend");
    let upstream = TestServer::start(mock_load_upstream::router(false, Duration::ZERO)).await?;
    let dir = tempfile::tempdir()?;
    let config = gateway_config(
        &upstream.url(),
        &dir.path().join("spool"),
        case.memory_bytes,
        case.retry,
    );
    let gateway =
        TestServer::start(llmux_server::app(RuntimeConfig::parse(&config)?).await?).await?;
    let client = client();
    let direct_uri = format!("{}/v1/chat/completions", upstream.url());
    let gateway_uri = format!("{}/v1/chat/completions", gateway.url());
    let direct_body = chat_body(BACKEND_MODEL, case.stream, case.target_bytes);
    let gateway_body = chat_body(GATEWAY_MODEL, case.stream, case.target_bytes);

    for _ in 0..5 {
        send(&client, &direct_uri, "mock-backend", &direct_body).await?;
        send(&client, &gateway_uri, CLIENT_KEY, &gateway_body).await?;
    }
    tokio::time::sleep(Duration::from_millis(5)).await;

    let mut measurements = Vec::new();
    let mut response_bytes = 0;
    let mut attempts_seen = None;
    for _ in 0..pairs {
        let (direct, direct_response, _) = measure_endpoint(
            &client,
            &direct_uri,
            "mock-backend",
            &direct_body,
            case.requests,
        )
        .await?;
        let (gateway, gateway_response, attempts) = measure_endpoint(
            &client,
            &gateway_uri,
            CLIENT_KEY,
            &gateway_body,
            case.requests,
        )
        .await?;
        response_bytes = response_bytes.max(direct_response).max(gateway_response);
        attempts_seen = attempts_seen.or(attempts);
        measurements.push(PairResult { direct, gateway });
    }

    let direct = median_batch(
        &measurements
            .iter()
            .map(|pair| pair.direct)
            .collect::<Vec<_>>(),
    );
    let gateway = median_batch(
        &measurements
            .iter()
            .map(|pair| pair.gateway)
            .collect::<Vec<_>>(),
    );
    let actual_attempts = attempts_seen.unwrap_or(1);
    if case.retry && actual_attempts != 2 {
        return Err(format!(
            "retry census did not exercise two attempts (x-llmux-attempts={actual_attempts})"
        )
        .into());
    }
    Ok(format!(
        "| {} | {} | {} | {} | {:.1} | {:.1} | {:+.1} | {:.0} | {:.0} | {:+.0} | {:.2} |\n",
        case.name,
        case.target_bytes,
        if case.stream { "SSE" } else { "JSON" },
        actual_attempts,
        direct.allocations,
        gateway.allocations,
        gateway.allocations - direct.allocations,
        direct.bytes,
        gateway.bytes,
        gateway.bytes - direct.bytes,
        (gateway.bytes - direct.bytes) / case.target_bytes.max(1) as f64,
    ))
}

fn deployment(upstream_model: &str) -> Deployment {
    Deployment {
        headers: HeaderMap::new(),
        fallback: true,
        idx: DeploymentIdx(0),
        id: Arc::from("copy-census"),
        provider: ProviderKind(Arc::from(provider::OPENAI_COMPAT)),
        credential: Arc::from("copy-census"),
        base_url: "http://127.0.0.1/v1".parse().expect("static uri"),
        upstream_model: Arc::from(upstream_model),
        strip_request_fields: Vec::new().into_boxed_slice(),
        priority: 0,
        quota: QuotaSpec::default(),
        shared_quota: None,
        timeout: Duration::from_secs(30),
        max_context: None,
        expires_at: None,
        priority_boost: None,
        priority_boost_until: None,
        billing: Billing::Billed,
    }
}

fn gateway_request(format: InboundFormat, body: Bytes) -> GatewayRequest {
    GatewayRequest {
        format,
        model: Arc::from(GATEWAY_MODEL),
        stream: false,
        est_input_tokens: (body.len() / 4).max(1) as u32,
        body,
        anthropic_headers: HeaderMap::new(),
        max_output_tokens: None,
        context_overflow: None,
        request_id: RequestId(Arc::from("copy-census")),
        end_user: None,
        client_key: None,
        image_count: 0,
        free_only: false,
    }
}

async fn measure_build(
    adapter: &dyn ProviderAdapter,
    name: &str,
    format: InboundFormat,
    body: Bytes,
    upstream_model: &str,
) -> Result<String> {
    let request = gateway_request(format, body);
    let deployment = deployment(upstream_model);
    let auth = AuthMaterial::Bearer(Arc::from("mock-backend"));
    let iterations = 100;
    let mut output_len = 0;
    for _ in 0..10 {
        let built = adapter.build_request(&request, &deployment, &auth).await?;
        output_len = built.body().len();
        std::hint::black_box(&built);
    }
    let start = alloc_counter::snapshot();
    for _ in 0..iterations {
        let built = adapter.build_request(&request, &deployment, &auth).await?;
        output_len = built.body().len();
        std::hint::black_box(&built);
    }
    let delta = alloc_counter::delta_since(start);
    Ok(format!(
        "| {name} | {} | {output_len} | {:.1} | {:.0} | {:.2} |\n",
        request.body.len(),
        delta.count as f64 / iterations as f64,
        delta.bytes as f64 / iterations as f64,
        delta.bytes as f64 / iterations as f64 / request.body.len().max(1) as f64,
    ))
}

fn measure_sync_parse(name: &str, body: &Bytes, value_dom: bool) -> String {
    let iterations = 100;
    for _ in 0..10 {
        if value_dom {
            std::hint::black_box(serde_json::from_slice::<serde_json::Value>(body).unwrap());
        } else {
            std::hint::black_box(RequestMetadata::parse(body, InboundFormat::OpenAiChat).unwrap());
        }
    }
    let start = alloc_counter::snapshot();
    for _ in 0..iterations {
        if value_dom {
            std::hint::black_box(serde_json::from_slice::<serde_json::Value>(body).unwrap());
        } else {
            std::hint::black_box(RequestMetadata::parse(body, InboundFormat::OpenAiChat).unwrap());
        }
    }
    let delta = alloc_counter::delta_since(start);
    format!(
        "| {name} | {:.1} | {:.0} | {:.2} |\n",
        delta.count as f64 / iterations as f64,
        delta.bytes as f64 / iterations as f64,
        delta.bytes as f64 / iterations as f64 / body.len().max(1) as f64,
    )
}

fn admission_parse_census() -> String {
    let body = chat_body(GATEWAY_MODEL, false, 64 * 1024);
    let mut rows = measure_sync_parse("RequestMetadata::parse (borrowed raw fields)", &body, false);
    rows.push_str(&measure_sync_parse(
        "serde_json::Value full-body parse (count_images shape)",
        &body,
        true,
    ));
    rows
}

async fn adapter_census() -> Result<String> {
    let adapter = OpenAiCompat::new(provider::OPENAI_COMPAT);
    let mut rows = String::new();
    for row in [
        measure_build(
            &adapter,
            "OpenAI chat passthrough, model rewrite",
            InboundFormat::OpenAiChat,
            chat_body(GATEWAY_MODEL, false, 64 * 1024),
            BACKEND_MODEL,
        )
        .await?,
        measure_build(
            &adapter,
            "OpenAI chat passthrough, same model control",
            InboundFormat::OpenAiChat,
            chat_body(GATEWAY_MODEL, false, 64 * 1024),
            GATEWAY_MODEL,
        )
        .await?,
        measure_build(
            &adapter,
            "OpenAI Responses → chat conversion",
            InboundFormat::OpenAiResponses,
            responses_body(GATEWAY_MODEL, 64 * 1024),
            BACKEND_MODEL,
        )
        .await?,
        measure_build(
            &adapter,
            "Anthropic Messages → chat conversion",
            InboundFormat::AnthropicMessages,
            anthropic_body(GATEWAY_MODEL, 64 * 1024),
            BACKEND_MODEL,
        )
        .await?,
    ] {
        rows.push_str(&row);
    }
    Ok(rows)
}

pub async fn run(_args: &[String]) -> Result<()> {
    let cases = [
        EndToEndCase {
            name: "small JSON, memory body",
            target_bytes: 256,
            stream: false,
            memory_bytes: 1024 * 1024,
            retry: false,
            requests: 100,
        },
        EndToEndCase {
            name: "64 KiB JSON, memory body",
            target_bytes: 64 * 1024,
            stream: false,
            memory_bytes: 1024 * 1024,
            retry: false,
            requests: 50,
        },
        EndToEndCase {
            name: "64 KiB JSON, spooled body",
            target_bytes: 64 * 1024,
            stream: false,
            memory_bytes: 1024,
            retry: false,
            requests: 50,
        },
        EndToEndCase {
            name: "64 KiB JSON, spooled body + retry",
            target_bytes: 64 * 1024,
            stream: false,
            memory_bytes: 1024,
            retry: true,
            requests: 40,
        },
        EndToEndCase {
            name: "small SSE stream",
            target_bytes: 256,
            stream: true,
            memory_bytes: 1024 * 1024,
            retry: false,
            requests: 40,
        },
    ];

    let mut end_to_end_rows = String::new();
    for case in cases {
        end_to_end_rows.push_str(&end_to_end_case(case, PAIRS).await?);
    }
    let build_rows = adapter_census().await?;
    let admission_rows = admission_parse_census();

    let mut output = String::new();
    writeln!(
        output,
        "copy/allocation census: in-process loopback, sequential requests, {PAIRS} paired batches; allocator counts are gross allocation requests, not memcpy counts."
    )
    .expect("string write");
    writeln!(
        output,
        "\n### End-to-end routed allocation census\n\n| Case | Request bytes | Shape | Attempts | Direct allocs/req | Via allocs/req | Δ allocs/req | Direct alloc bytes/req | Via alloc bytes/req | Δ alloc bytes/req | Δ bytes ÷ request bytes |\n| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n{end_to_end_rows}"
    )
    .expect("string write");
    writeln!(
        output,
        "\n### Provider `build_request` allocation census (64 KiB inputs)\n\n| Case | Input bytes | Output bytes | Allocs/op | Alloc bytes/op | Alloc bytes ÷ input bytes |\n| --- | ---: | ---: | ---: | ---: | ---: |\n{build_rows}"
    )
    .expect("string write");
    writeln!(
        output,
        "\n### Admission parse allocation census (64 KiB OpenAI chat input)\n\n| Operation | Allocs/op | Alloc bytes/op | Alloc bytes ÷ input bytes |\n| --- | ---: | ---: | ---: |\n{admission_rows}"
    )
    .expect("string write");
    writeln!(
        output,
        "\nValidation: every request returned 2xx; retry case asserted `x-llmux-attempts: 2`. The direct leg uses the same mock upstream and final backend model, so subtraction isolates gateway work in steady state."
    )
    .expect("string write");
    print!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn padded_chat_body_is_valid_json_at_target_size() {
        for target in [256, 4096] {
            let body = chat_body(GATEWAY_MODEL, false, target);
            assert_eq!(body.len(), target);
            let value: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value["model"], GATEWAY_MODEL);
            assert_eq!(value["stream"], false);
        }
    }

    #[test]
    fn conversion_bodies_are_valid_json() {
        let responses: Value =
            serde_json::from_slice(&responses_body(GATEWAY_MODEL, 1024)).unwrap();
        assert_eq!(responses["model"], GATEWAY_MODEL);
        let anthropic: Value =
            serde_json::from_slice(&anthropic_body(GATEWAY_MODEL, 1024)).unwrap();
        assert_eq!(anthropic["messages"][0]["role"], "user");
    }

    #[test]
    fn median_batch_uses_middle_values() {
        let batch = median_batch(&[
            Batch {
                allocations: 1.0,
                bytes: 10.0,
            },
            Batch {
                allocations: 3.0,
                bytes: 30.0,
            },
            Batch {
                allocations: 2.0,
                bytes: 20.0,
            },
        ]);
        assert_eq!(batch.allocations, 2.0);
        assert_eq!(batch.bytes, 20.0);
    }

    #[tokio::test]
    async fn spooled_retry_census_and_isolated_parser_measurements_smoke() {
        let row = end_to_end_case(
            EndToEndCase {
                name: "test spooled retry",
                target_bytes: 2048,
                stream: false,
                memory_bytes: 128,
                retry: true,
                requests: 2,
            },
            1,
        )
        .await
        .unwrap();
        assert!(row.contains("test spooled retry"));
        assert!(row.contains("| 2 |"));

        let build_rows = adapter_census().await.unwrap();
        assert!(build_rows.contains("OpenAI Responses → chat conversion"));
        assert!(build_rows.contains("Anthropic Messages → chat conversion"));

        let admission_rows = admission_parse_census();
        assert!(admission_rows.contains("RequestMetadata::parse"));
        assert!(admission_rows.contains("serde_json::Value full-body parse"));
    }
}
