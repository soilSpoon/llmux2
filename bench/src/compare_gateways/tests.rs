use super::*;

#[test]
fn timestamps_preserve_utc_offset_and_grouped_fractional_precision() {
    for (nanos, suffix) in [
        (0, "+00:00"),
        (120_000_000, ".120+00:00"),
        (123_400_000, ".123400+00:00"),
        (123_456_789, ".123456789+00:00"),
    ] {
        assert_eq!(timestamp(0, nanos), format!("1970-01-01T00:00:00{suffix}"));
    }
}
fn gateways() -> Vec<String> {
    vec!["omniroute".into(), "llmux".into()]
}
fn result(gateway: &str, kind: &str, error: &str, latency: f64) -> Value {
    json!({"gateway":gateway,"model":"m","kind":kind,"error_class":error,"status":if error=="ok" {200} else {429},"latency_ms":latency,"ttft_ms":null,"done":null,"deployment":null,"attempts":null,"fallback":null,"routing_headers":{},"error":null})
}
#[test]
fn report_has_compact_summary_model_overview_and_run_list() {
    let rows = vec![
        result("omniroute", "chat", "ok", 10.0),
        result("llmux", "chat", "ok", 10.0),
        result("omniroute", "stream", "ok", 10.0),
        result("llmux", "stream", "rate_limited", 10.0),
    ];
    let mut report = aggregate(&rows, &gateways()).unwrap();
    report["run"] = json!({"completed_at":"2026-11-20T09:00:00Z","timeout_s":45,"max_requests":80,"note":"A single sample"});
    let source = render_html(&report);
    for s in [
        "aria-label=\"Summary\"",
        "Successes / requests",
        "2 / 2",
        "1 / 2",
        "rate_limited: 1",
        "p50 all (ms)",
        "p50 success (ms)",
        "p50 TTFT (ms)",
        "200 / 200",
        "200 / 429",
        "llmux-only fail",
        "<dl",
        "A single sample",
        "match: 1",
        "llmux-only fail: 1",
        "both fail: 0",
    ] {
        assert!(source.contains(s), "missing {s}: {source}");
    }
    assert!(
        source.find("aria-label=\"Models\"").unwrap()
            < source.find("aria-label=\"Request details\"").unwrap()
    );
    assert!(!source.contains("&quot;completed_at&quot;"));
}
#[test]
fn case_cells_show_human_values_and_only_candidate_attempt_evidence() {
    let mut rows = vec![
        result("omniroute", "chat", "ok", 10.0),
        result("llmux", "chat", "ok", 10.0),
        result("omniroute", "stream", "ok", 10.0),
        result("llmux", "stream", "ok", 10.0),
    ];
    rows[0]["attempts"] = json!(7);
    rows[1]["attempts"] = json!(1);
    rows[1]["deployment"] = json!("winner");
    rows[2]["done"] = json!(false);
    rows[3]["done"] = json!(true);
    rows[3]["attempts"] = json!(2);
    rows[3]["ttft_ms"] = json!(5);
    let html = render_html(&aggregate(&rows, &gateways()).unwrap());
    for s in [
        "1 attempt",
        "winner",
        "DONE ✗",
        "DONE ✓",
        "2 attempts (fallback)",
    ] {
        assert!(html.contains(s), "{html}");
    }
    for s in ["7 attempts", "None", "True", "False"] {
        assert!(!html.contains(s));
    }
}
#[test]
fn case_bytes_are_fixed_and_pairs_alternate_before_cap_check() {
    let cases = generate_cases(&json!(["m", "m", "n"]), 2).unwrap();
    assert_eq!(
        cases
            .iter()
            .map(|c| (c["model"].as_str().unwrap(), c["kind"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        [
            ("m", "chat"),
            ("m", "stream"),
            ("n", "chat"),
            ("n", "stream"),
            ("m", "responses"),
            ("n", "responses")
        ]
    );
    let body: Value = serde_json::from_str(cases[0]["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["max_tokens"], 16);
    assert_eq!(body["stream"], false);
    let plan = make_plan(&cases, &gateways(), 12).unwrap();
    assert_eq!(&plan[..4], &[(0, 0), (0, 1), (1, 1), (1, 0)]);
    assert!(make_plan(&cases, &gateways(), 11).is_err());
}
#[test]
fn empty_models_bad_cap_and_response_count_are_rejected() {
    for (models, count) in [
        (json!([]), 0),
        (json!(["m"]), -1),
        (json!(["m"]), 2),
        (json!([{}]), 0),
    ] {
        assert!(generate_cases(&models, count).is_err());
    }
    assert!(make_plan(&generate_cases(&json!(["m"]), 0).unwrap(), &gateways(), 0).is_err());
}
#[test]
fn coverage_filters_imported_names_after_top_traffic_cutoff() {
    assert_eq!(coverage_models(&json!({"rows":[{"name":"low","calls":1,"present":true},{"name":"missing","calls":99,"present":false},{"name":"used","calls":50,"present":true}]}),2).unwrap(),["used"]);
    assert!(coverage_models(&json!({"rows":[]}), 25).unwrap().is_empty());
    for data in [json!({"rows":"invalid"}), json!([])] {
        assert!(coverage_models(&data, 25).is_err());
    }
}
#[test]
fn result_classes_do_not_confuse_fast_errors_with_success() {
    for (status, code, transport, expected) in [
        (Some(200), None, None, "ok"),
        (Some(429), None, None, "rate_limited"),
        (
            Some(400),
            Some("context_length_exceeded"),
            None,
            "gateway_reject",
        ),
        (Some(404), Some("unknown_model"), None, "gateway_reject"),
        (Some(404), Some("model_not_found"), None, "upstream_4xx"),
        (Some(502), None, None, "upstream_5xx"),
        (Some(200), Some("503"), None, "upstream_5xx"),
        (None, None, Some("timeout"), "timeout"),
        (Some(200), None, Some("connection"), "connection"),
    ] {
        assert_eq!(classify(status, code, transport), expected);
    }
}
#[tokio::test]
async fn cli_cap_is_checked_before_any_request_or_output() {
    let directory = crate::process::Directory::new().unwrap();
    let key = directory.0.join("key");
    std::fs::write(&key, "test-key").unwrap();
    let output = directory.0.join("run");
    let args: Vec<_> = vec![
        "--gateway".into(),
        "omniroute".into(),
        "http://127.0.0.1:1/v1".into(),
        format!("file:{}", key.display()),
        "--gateway".into(),
        "llmux".into(),
        "http://127.0.0.1:2/v1".into(),
        format!("file:{}", key.display()),
        "--model".into(),
        "m".into(),
        "--max-requests".into(),
        "3".into(),
        "--output".into(),
        output.to_string_lossy().into_owned(),
    ];
    assert!(run(&args).await.is_err());
    assert!(!output.exists());
}
#[test]
fn routing_header_filter_keeps_discovered_fallback_but_not_secrets() {
    let safe = routing_headers(
        &[
            ("x-omniroute-fallback-used", "true"),
            ("x-api-key", "private"),
            ("set-cookie", "secret"),
            ("x-omniroute-token", "private"),
            ("x-omniroute-provider", "provider"),
            ("x-omniroute-route", "https://user:pass@host/private"),
        ],
        &[],
    );
    assert_eq!(safe["x-omniroute-fallback-used"], "true");
    assert!(!safe.to_string().contains("private"));
    assert!(!safe.to_string().contains("user:pass"));
    assert!(
        routing_headers(&[("x-omniroute-agent-goal", "must not store prompts")], &[])
            .get("x-omniroute-agent-goal")
            .is_none()
    );
}
#[test]
fn long_deployment_names_are_retained_while_opaque_credentials_are_removed() {
    let safe = routing_headers(
        &[
            ("x-llmux-deployment", "deepseek-v4-flash-fast-deployment-0"),
            ("x-omniroute-provider", "abcdefghijklmnopqrstuvwxy123456"),
        ],
        &[],
    );
    assert_eq!(
        safe["x-llmux-deployment"],
        "deepseek-v4-flash-fast-deployment-0"
    );
    assert!(!safe.to_string().contains("abcdefghijklmnopqrstuvwxy123456"));
}
#[test]
fn redaction_and_html_escape_untrusted_errors_and_headers() {
    let secret = "known-test-secret-value";
    let text = format!(
        "Bearer abcdefghijklmnop sk-live1234567890 https://user:pass@host/path?key=x {secret}"
    );
    let clean = redact(&text, &[secret.into()]);
    for s in [
        secret,
        "abcdefghijklmnop",
        "sk-live1234567890",
        "user:pass",
        "key=x",
    ] {
        assert!(!clean.contains(s), "{clean}");
    }
    let mut rows = vec![
        result("omniroute", "chat", "ok", 10.0),
        result("llmux", "chat", "rate_limited", 10.0),
    ];
    rows[1]["error"] = json!("<script>alert(\"x\")</script>");
    let mut report = aggregate(&rows, &gateways()).unwrap();
    report["run"] = json!({"note":"Responses pairs rechecked after classifier correction"});
    let html = render_html(&report);
    assert!(!html.contains("<script>alert"));
    assert!(html.contains("&lt;script&gt;"));
    assert!(html.contains("llmux-only fail"));
    assert!(!html.contains("src=\"http"));
    assert!(html.contains("Responses pairs rechecked after classifier correction"));
}
#[test]
fn aggregation_keeps_success_latency_separate_and_preserves_each_case() {
    let rows = vec![
        result("omniroute", "chat", "ok", 10.0),
        result("llmux", "chat", "ok", 20.0),
        result("omniroute", "stream", "timeout", 1000.0),
        result("llmux", "stream", "ok", 30.0),
    ];
    let report = aggregate(&rows, &gateways()).unwrap();
    assert_eq!(
        report["summary"]["verdicts"],
        json!({"match":1,"omniroute-only fail":1})
    );
    assert_eq!(
        report["summary"]["gateways"]["omniroute"]["success_p50_ms"],
        10.0
    );
    assert_eq!(report["summary"]["gateways"]["omniroute"]["p50_ms"], 505.0);
    assert_eq!(report["models"][0]["verdict"], "omniroute-only fail");
    assert_eq!(report["pairs"].as_array().unwrap().len(), 2);
    let empty = aggregate(&[], &gateways()).unwrap();
    assert!(empty["summary"]["gateways"]["llmux"]["p50_ms"].is_null());
    assert!(render_html(&empty).contains("No results"));
    assert!(aggregate(&[result("omniroute", "chat", "ok", 10.0)], &gateways()).is_err());
}
#[test]
fn both_failure_and_mixed_direction_verdicts_are_not_matches() {
    let names = vec!["a".into(), "b".into()];
    assert_eq!(
        aggregate(
            &[
                result("a", "chat", "timeout", 10.0),
                result("b", "chat", "rate_limited", 10.0)
            ],
            &names
        )
        .unwrap()["pairs"][0]["verdict"],
        "both fail"
    );
    assert_eq!(
        aggregate(
            &[
                result("a", "chat", "ok", 10.0),
                result("b", "chat", "timeout", 10.0),
                result("a", "stream", "timeout", 10.0),
                result("b", "stream", "ok", 10.0)
            ],
            &names
        )
        .unwrap()["models"][0]["verdict"],
        "both fail"
    );
}
#[test]
fn secret_inputs_reject_literals_and_urls_reject_userinfo_query() {
    let directory = crate::process::Directory::new().unwrap();
    let file = directory.0.join("key");
    std::fs::write(&file, "secret-value\n").unwrap();
    assert_eq!(
        read_key(&format!("file:{}", file.display())).unwrap(),
        "secret-value"
    );
    std::env::set_var("LLMUX_BENCH_COMPARE_TEST_KEY", "env-secret");
    assert_eq!(
        read_key("env:LLMUX_BENCH_COMPARE_TEST_KEY").unwrap(),
        "env-secret"
    );
    for key in ["literal-secret", "env:UNSET_COMPARE_KEY"] {
        assert!(read_key(key).is_err());
    }
    for url in [
        "http://user:pass@localhost/v1",
        "http://localhost/v1?key=oops",
        "ftp://localhost",
    ] {
        assert!(validate_url(url).is_err());
    }
}

async fn fixture() -> (
    Value,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let store = received.clone();
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let store = store.clone();
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut chunk = [0; 4096];
                let end = loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&chunk[..n]);
                    if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                while data.len() - end < length {
                    let n = socket.read(&mut chunk).await.unwrap();
                    data.extend_from_slice(&chunk[..n]);
                }
                let body = data[end..].to_vec();
                let request: Value = serde_json::from_slice(&body).unwrap();
                store.lock().unwrap().push(body);
                let model = request["model"].as_str().unwrap();
                let stream = request["stream"] == true;
                if matches!(model, "short-body" | "sized-body") {
                    let payload = if stream {
                        "data: {\"choices\":[{\"delta\":{}}]}\n\ndata: [DONE]\n\n"
                    } else {
                        "{\"choices\":[{\"message\":{}}]}"
                    };
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len() + if model == "short-body" { 10 } else { 0 }
                    );
                    let _ = socket
                        .write_all(format!("{header}{payload}").as_bytes())
                        .await;
                    return;
                }
                let _=socket.write_all(b"HTTP/1.1 200 OK\r\nx-llmux-deployment: deployment-1\r\nx-llmux-attempts: 2\r\nx-omniroute-route-class: inference\r\nx-api-key: never-store-this\r\nConnection: close\r\n\r\n").await;
                if model == "slow" {
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
                let payload = if stream {
                    match model {"invalid-delta"=>"data: {\"choices\":[{\"delta\":1}]}\n\ndata: [DONE]\n\n".into(),"cr-only"=>"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\r\rdata: [DONE]\r\r".into(),"bom-error"=>"\u{feff}data: {\"error\":{\"code\":\"unknown_model\",\"message\":\"rejected\"}}\r\rdata: [DONE]\r\r".into(),"malformed"=>"data: {\"choices\":1}\n\ndata: [DONE]\n\n".into(),_=>{let _=socket.write_all(b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\r\n\r\n").await;tokio::time::sleep(std::time::Duration::from_millis(20)).await;format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"secret-response-text\"}}}}]}}\r\n\r\n{}",if model=="broken" {""} else {"data: [DONE]\r\n\r\n"})}}
                } else if matches!(model, "scalar-items" | "empty-items") {
                    json!({if headers.starts_with("post /v1/responses") {"output"} else {"choices"}: if model=="scalar-items" {json!([1])} else {json!([])}}).to_string()
                } else if model == "bad" {
                    json!({"error":{"message":"sk-secret1234567890123456789","code":"unknown_model"}}).to_string()
                } else if headers.starts_with("post /v1/responses") {
                    json!({"error":null,"output":[{"type":"message","content":[{"text":"secret-response-text"}]}]}).to_string()
                } else {
                    json!({"choices":[{"message":{"content":"secret-response-text"}}]}).to_string()
                };
                let _ = socket.write_all(payload.as_bytes()).await;
            });
        }
    });
    (
        json!({"name":"llmux","base_url":format!("http://{address}/v1"),"key":"test-client-key"}),
        task,
        received,
    )
}
async fn row(gateway: &Value, model: &str, index: usize, timeout: f64) -> Value {
    send(
        gateway,
        &generate_cases(&json!([model]), 1).unwrap()[index],
        timeout,
        &["test-client-key".into()],
    )
    .await
}
#[tokio::test]
async fn identical_bytes_headers_and_content_ttft_without_content_retention() {
    let (gateway, task, received) = fixture().await;
    let a = row(&gateway, "m", 1, 2.0).await;
    let b = row(&gateway, "m", 1, 2.0).await;
    let body = generate_cases(&json!(["m"]), 0).unwrap()[1]["body"]
        .as_str()
        .unwrap()
        .as_bytes()
        .to_vec();
    assert_eq!(*received.lock().unwrap(), [body.clone(), body]);
    assert_eq!(a["error_class"], "ok");
    assert_eq!(a["done"], true);
    assert!(a["ttft_ms"].as_f64().unwrap() >= 15.0);
    assert!(a["latency_ms"].as_f64().unwrap() >= a["ttft_ms"].as_f64().unwrap());
    assert_eq!(a["fallback"], true);
    assert_eq!(a["routing_headers"]["x-omniroute-route-class"], "inference");
    assert!(!a.to_string().contains("never-store-this"));
    assert!(!json!([a, b]).to_string().contains("secret-response-text"));
    task.abort();
}
#[tokio::test]
async fn incomplete_stream_and_error_in_http_200_are_failures() {
    let (g, t, _) = fixture().await;
    let broken = row(&g, "broken", 1, 2.0).await;
    assert_eq!(broken["error_class"], "connection");
    assert_eq!(broken["done"], false);
    let bad = row(&g, "bad", 0, 2.0).await;
    assert_eq!(bad["error_class"], "gateway_reject");
    assert!(!bad.to_string().contains("sk-secret"));
    t.abort();
}
#[tokio::test]
async fn deadline_bounds_wait_for_body_and_does_not_reclassify_http_200_as_ok() {
    let (g, t, _) = fixture().await;
    let r = row(&g, "slow", 0, 0.03).await;
    assert_eq!(r["status"], 200);
    assert_eq!(r["error_class"], "timeout");
    assert!(r["latency_ms"].as_f64().unwrap() < 140.0);
    t.abort();
}
#[tokio::test]
async fn malformed_sse_is_a_result_not_a_crashed_comparison() {
    let (g, t, _) = fixture().await;
    let r = row(&g, "malformed", 1, 2.0).await;
    assert_eq!(r["status"], 200);
    assert_eq!(r["error_class"], "connection");
    t.abort();
}
#[tokio::test]
async fn cr_only_events_complete_and_leading_bom_does_not_hide_errors() {
    let (g, t, _) = fixture().await;
    let r = row(&g, "cr-only", 1, 2.0).await;
    assert_eq!(r["error_class"], "ok");
    assert_eq!(r["done"], true);
    assert!(!r["ttft_ms"].is_null());
    let r = row(&g, "bom-error", 1, 2.0).await;
    assert_eq!(r["error_class"], "gateway_reject");
    assert_eq!(r["done"], true);
    t.abort();
}
#[tokio::test]
async fn successful_responses_with_null_error_is_not_a_failure() {
    let (g, t, _) = fixture().await;
    let r = row(&g, "m", 2, 2.0).await;
    assert_eq!(r["status"], 200);
    assert_eq!(r["error_class"], "ok");
    assert!(r["error"].is_null());
    assert!(!r.to_string().contains("secret-response-text"));
    t.abort();
}
#[tokio::test]
async fn invalid_success_collections_and_stream_deltas_fail() {
    let (g, t, _) = fixture().await;
    for m in ["scalar-items", "empty-items"] {
        for i in [0, 2] {
            assert_eq!(row(&g, m, i, 2.0).await["error_class"], "connection");
        }
    }
    assert_eq!(
        row(&g, "invalid-delta", 1, 2.0).await["error_class"],
        "connection"
    );
    t.abort();
}
#[tokio::test]
async fn content_length_requires_complete_body_even_after_stream_done() {
    let (g, t, _) = fixture().await;
    for (m, expected) in [("short-body", "connection"), ("sized-body", "ok")] {
        for i in [0, 1] {
            let r = row(&g, m, i, 2.0).await;
            assert_eq!(r["status"], 200);
            assert_eq!(r["error_class"], expected);
        }
    }
    t.abort();
}
