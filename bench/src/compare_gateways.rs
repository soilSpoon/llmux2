use crate::Result;
use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, Request};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

const MAX_BODY: usize = 1024 * 1024;
const MAX_EVENT: usize = 64 * 1024;
const PROMPT: &str = "Reply with just OK.";

fn generate_cases(models: &Value, responses: i64) -> Result<Vec<Value>> {
    let models = models.as_array().ok_or("models JSON must be an array")?;
    if models.is_empty()
        || models
            .iter()
            .any(|v| v.as_str().is_none_or(|s| s.trim().is_empty()))
    {
        return Err("provide at least one nonempty model name".into());
    }
    let mut unique = Vec::new();
    for model in models {
        if !unique.contains(model) {
            unique.push(model.clone());
        }
    }
    if responses < 0 || responses as usize > unique.len() {
        return Err("responses count must be between zero and the model count".into());
    }
    let mut cases = Vec::new();
    for model in &unique {
        for kind in ["chat", "stream"] {
            let body = format!(
                r#"{{"model":{model},"messages":[{{"role":"user","content":"{PROMPT}"}}],"max_tokens":16,"stream":{}}}"#,
                kind == "stream"
            );
            cases.push(json!({"model":model,"kind":kind,"path":"/chat/completions","body":body}));
        }
    }
    for model in unique.iter().take(responses as usize) {
        cases.push(json!({"model":model,"kind":"responses","path":"/responses","body":format!(r#"{{"model":{model},"input":"{PROMPT}","max_output_tokens":16}}"#)}));
    }
    Ok(cases)
}
fn make_plan(cases: &[Value], gateways: &[String], cap: usize) -> Result<Vec<(usize, usize)>> {
    if gateways.len() != 2 || gateways[0] == gateways[1] {
        return Err("provide two distinct gateway names".into());
    }
    if cap < 1 || cases.len().saturating_mul(2) > cap {
        return Err("over-budget plan (nothing sent)".into());
    }
    Ok((0..cases.len())
        .flat_map(|i| {
            if i.is_multiple_of(2) {
                [(i, 0), (i, 1)]
            } else {
                [(i, 1), (i, 0)]
            }
        })
        .collect())
}
fn coverage_models(data: &Value, top: usize) -> Result<Vec<String>> {
    let rows = data["rows"]
        .as_array()
        .filter(|_| top > 0)
        .ok_or("coverage/traffic JSON needs a rows array and positive --top")?;
    if rows
        .iter()
        .any(|r| !r.is_object() || !r["name"].is_string() || !r["calls"].is_number())
    {
        return Err("coverage rows need name and calls".into());
    }
    let mut ranked = rows.clone();
    ranked.sort_by(|a, b| {
        b["calls"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["calls"].as_f64().unwrap_or(0.0))
    });
    Ok(ranked
        .iter()
        .take(top)
        .filter(|r| r.get("present").is_none_or(truthy))
        .filter_map(|r| r["name"].as_str().map(str::to_owned))
        .collect())
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
    }
}
fn classify(status: Option<u16>, code: Option<&str>, transport: Option<&str>) -> String {
    if let Some(transport) = transport {
        return transport.into();
    }
    let has_code = code.is_some();
    let code = code.unwrap_or("");
    if status == Some(429)
        || [
            "429",
            "rate_limit_exceeded",
            "rate_limited",
            "insufficient_quota",
        ]
        .contains(&code)
    {
        "rate_limited"
    } else if matches!(status, Some(401 | 403))
        || [
            "unknown_model",
            "context_length_exceeded",
            "invalid_api_key",
            "unauthorized",
            "unsupported_format",
        ]
        .contains(&code)
    {
        "gateway_reject"
    } else if status.is_some_and(|s| s >= 500)
        || ["500", "502", "503", "504", "server_error"].contains(&code)
    {
        "upstream_5xx"
    } else if has_code || status.is_some_and(|s| s >= 400) {
        "upstream_4xx"
    } else if status.is_some_and(|s| (200..300).contains(&s)) {
        "ok"
    } else {
        "gateway_reject"
    }
    .into()
}
fn routing_headers(headers: &[(&str, &str)], secrets: &[String]) -> Value {
    const ALLOWED: &[&str] = &[
        "x-llmux-deployment",
        "x-llmux-attempts",
        "x-llmux-dropped-messages",
        "x-request-id",
        "x-provider",
        "x-model",
        "x-combo",
        "x-fallback",
        "x-fallback-count",
        "x-upstream-provider",
        "x-model-alias-resolved",
        "x-omniroute-route-class",
        "x-omniroute-route-decision",
        "x-omniroute-routed-by",
        "x-omniroute-fallback-attempts",
        "x-omniroute-fallback-used",
        "x-omniroute-emergency-fallback",
        "x-omniroute-reasoning-fallback",
        "x-omniroute-budget-fallback",
        "x-omniroute-provider",
        "x-omniroute-model",
        "x-omniroute-connection",
        "x-omniroute-selected-connection-id",
        "x-omniroute-combo-attempted",
        "x-omniroute-combo-excluded",
        "x-omniroute-combo-pool-size",
        "x-omniroute-combo-terminal-reason",
        "x-omniroute-combo-trace",
        "x-omniroute-local-cooldown",
        "x-omniroute-retry-after-seconds",
        "x-omniroute-decision",
    ];
    Value::Object(
        headers
            .iter()
            .filter_map(|(key, value)| {
                let key = key.to_lowercase();
                ALLOWED
                    .contains(&key.as_str())
                    .then(|| (key, Value::String(redact(value, secrets))))
            })
            .collect(),
    )
}
fn token_end(c: char) -> bool {
    c.is_whitespace() || ",;\"'".contains(c)
}
fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
fn replace_matches<F: Fn(&str, usize) -> Option<(usize, &'static str)>>(
    text: &str,
    find: F,
) -> String {
    let mut out = String::new();
    let mut index = 0;
    while index < text.len() {
        if let Some((end, replacement)) = find(text, index) {
            out.push_str(replacement);
            index = end;
        } else {
            let c = text[index..].chars().next().expect("nonempty suffix");
            out.push(c);
            index += c.len_utf8();
        }
    }
    out
}
fn redact(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_owned();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        text = text.replace(secret, "[REDACTED]");
    }
    text = replace_matches(&text, |s, i| {
        let rest = &s[i..];
        if rest
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
            || rest
                .get(..8)
                .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
        {
            let end = rest
                .find(|c: char| c.is_whitespace() || "<>\"'".contains(c))
                .unwrap_or(rest.len());
            return Some((i + end, "[URL]"));
        }
        None
    });
    text = replace_matches(&text, |s, i| {
        if i > 0 && s[..i].ends_with(word) {
            return None;
        }
        let rest = &s[i..];
        let lower = rest.to_ascii_lowercase();
        for prefix in ["bearer", "basic"] {
            if let Some(suffix) = lower.strip_prefix(prefix) {
                if suffix.starts_with(char::is_whitespace) {
                    let offset = rest.len() - rest[prefix.len()..].trim_start().len();
                    let len = rest[offset..]
                        .find(token_end)
                        .unwrap_or(rest.len() - offset);
                    if len > 0 {
                        return Some((i + offset + len, "[REDACTED]"));
                    }
                }
            }
        }
        for prefix in [
            "sk", "pk", "api-key", "api_key", "apikey", "token", "secret",
        ] {
            if let Some(suffix) = lower.strip_prefix(prefix) {
                if suffix.starts_with(['-', '_', ':']) {
                    let offset = prefix.len() + 1;
                    let len = rest[offset..]
                        .find(|c: char| !c.is_ascii_alphanumeric() && !"_./+=-".contains(c))
                        .unwrap_or(rest.len() - offset);
                    if len > 0 {
                        return Some((i + offset + len, "[REDACTED]"));
                    }
                }
            }
        }
        for prefix in ["key", "token", "secret", "authorization"] {
            if lower.starts_with(prefix) {
                let suffix = rest[prefix.len()..].trim_start();
                if suffix.starts_with(['=', ':']) {
                    let value = suffix[1..].trim_start();
                    let offset = rest.len() - value.len();
                    let len = value.find(token_end).unwrap_or(value.len());
                    if len > 0 {
                        return Some((i + offset + len, "[REDACTED]"));
                    }
                }
            }
        }
        let len = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && !"_+/=".contains(c))
            .unwrap_or(rest.len());
        (len >= 24 && rest[..len].ends_with(word)).then_some((i + len, "[REDACTED]"))
    });
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(400)
        .collect()
}
pub fn read_key(reference: &str) -> Result<String> {
    let value = if let Some(path) = reference.strip_prefix("file:") {
        fs::read_to_string(path)?
    } else if let Some(name) = reference.strip_prefix("env:") {
        std::env::var(name).unwrap_or_default()
    } else {
        return Err("gateway key must use file:PATH or env:VARIABLE, never a literal".into());
    };
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|c| (33..=126).contains(&c)) {
        return Err("missing or invalid gateway key".into());
    }
    Ok(value.to_owned())
}
fn validate_url(url: &str) -> Result<url::Url> {
    let parsed = url::Url::parse(url)?;
    if !["http", "https"].contains(&parsed.scheme())
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("base URL must be HTTP(S), without credentials, query or fragment".into());
    }
    Ok(parsed)
}
fn verdict(a: bool, b: bool) -> &'static str {
    match (a, b) {
        (true, true) => "match",
        (true, false) => "llmux-only fail",
        (false, true) => "omniroute-only fail",
        _ => "both fail",
    }
}
fn mid(values: impl Iterator<Item = f64>) -> Value {
    let mut values: Vec<_> = values.collect();
    values.sort_by(f64::total_cmp);
    if values.is_empty() {
        Value::Null
    } else {
        json!(round(
            (values[(values.len() - 1) / 2] + values[values.len() / 2]) / 2.0
        ))
    }
}
fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}
fn aggregate(rows: &[Value], gateways: &[String]) -> Result<Value> {
    if gateways.len() != 2 {
        return Err("two gateways required".into());
    }
    let mut pairs: Vec<Value> = Vec::new();
    for row in rows {
        let model = row["model"].as_str().ok_or("missing model")?;
        let kind = row["kind"].as_str().ok_or("missing kind")?;
        let name = row["gateway"].as_str().ok_or("missing gateway")?;
        if !gateways.iter().any(|g| g == name) {
            return Err("unknown gateway".into());
        }
        let i = if let Some(i) = pairs
            .iter()
            .position(|p| p["model"] == model && p["kind"] == kind)
        {
            i
        } else {
            pairs.push(json!({"model":model,"kind":kind,"results":{}}));
            pairs.len() - 1
        };
        if pairs[i]["results"].get(name).is_some() {
            return Err("duplicate result".into());
        }
        pairs[i]["results"][name] = row.clone();
    }
    let mut counts = BTreeMap::<String, usize>::new();
    let mut models: Vec<Value> = Vec::new();
    for pair in &mut pairs {
        let a = pair["results"]
            .get(&gateways[0])
            .ok_or("unpaired results")?;
        let b = pair["results"]
            .get(&gateways[1])
            .ok_or("unpaired results")?;
        let v = verdict(a["error_class"] == "ok", b["error_class"] == "ok");
        let same = a["error_class"] == b["error_class"];
        pair["verdict"] = json!(v);
        pair["same_error_class"] = json!(same);
        *counts.entry(v.into()).or_default() += 1;
        let i = if let Some(i) = models.iter().position(|m| m["model"] == pair["model"]) {
            i
        } else {
            models.push(json!({"model":pair["model"],"cases":[]}));
            models.len() - 1
        };
        models[i]["cases"]
            .as_array_mut()
            .ok_or("invalid cases")?
            .push(pair.clone());
    }
    for model in &mut models {
        let cases = model["cases"].as_array().ok_or("invalid cases")?;
        let ok: Vec<_> = gateways
            .iter()
            .map(|g| cases.iter().all(|c| c["results"][g]["error_class"] == "ok"))
            .collect();
        model["verdict"] = json!(verdict(ok[0], ok[1]));
    }
    let mut summaries = serde_json::Map::new();
    for gateway in gateways {
        let own: Vec<_> = rows.iter().filter(|r| r["gateway"] == *gateway).collect();
        let successful: Vec<_> = own
            .iter()
            .copied()
            .filter(|r| r["error_class"] == "ok")
            .collect();
        let mut errors = BTreeMap::<String, usize>::new();
        for row in own.iter().filter(|r| r["error_class"] != "ok") {
            *errors
                .entry(row["error_class"].as_str().unwrap_or("connection").into())
                .or_default() += 1;
        }
        summaries.insert(gateway.clone(),json!({"requests":own.len(),"successes":successful.len(),"errors":errors,"p50_ms":mid(own.iter().filter_map(|r|r["latency_ms"].as_f64())),"success_p50_ms":mid(successful.iter().filter_map(|r|r["latency_ms"].as_f64())),"success_ttft_p50_ms":mid(successful.iter().filter_map(|r|r["ttft_ms"].as_f64()))}));
    }
    Ok(
        json!({"gateways":gateways,"summary":{"requests":rows.len(),"pairs":pairs.len(),"verdicts":counts,"gateways":summaries},"models":models,"pairs":pairs}),
    )
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}
fn display(value: &Value) -> String {
    match value {
        Value::Null => "—".into(),
        Value::String(s) => escape(s),
        Value::Number(n) => {
            let s = if let Some(v) = n.as_f64().filter(|_| n.is_f64()) {
                format!("{v:.3}")
                    .trim_end_matches('0')
                    .trim_end_matches('.')
                    .to_owned()
            } else {
                n.to_string()
            };
            escape(&s)
        }
        _ => escape(&value.to_string()),
    }
}
fn badge(v: &str, count: Option<usize>) -> String {
    format!(
        "<span class=\"badge {}\">{}{}</span>",
        if v == "match" { "ok" } else { "fail" },
        escape(v),
        count.map(|n| format!(": {n}")).unwrap_or_default()
    )
}
fn render_html(report: &Value) -> String {
    let gateways: Vec<_> = report["gateways"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut html=String::from("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Gateway comparison</title><style>body{font:15px system-ui,sans-serif;margin:2rem;color:#17212b;background:#f6f8fa}table{border-collapse:collapse;width:100%;background:white;margin:1rem 0}th,td{border:1px solid #c9d1d9;padding:.65rem;text-align:left;vertical-align:top}small{display:block;overflow-wrap:anywhere}pre{white-space:pre-wrap}.ok{color:#176734}.fail{color:#a32620}.badge{display:inline-block;padding:.15rem .45rem;border-radius:.3rem}.badge.ok{background:#e0f2e5}.badge.fail{background:#fde7e5}.strip{display:flex;gap:1rem;flex-wrap:wrap}dl{display:grid;grid-template-columns:max-content 1fr;gap:.35rem 1rem}dd{margin:0;overflow-wrap:anywhere}h1{margin-bottom:.4rem}caption{text-align:left;color:#52606d;margin:.3rem 0}</style><h1>Gateway comparison</h1><p>Single sequential snapshot, not a performance ranking. Both gateways receive identical bytes; pair order alternates. No automatic retries. Fast errors are not successful work. TTFT starts at request dispatch and ends at the first content delta.</p><p>Verdicts use success across each pair; both failures are never a match, even with equal error classes. Model verdict requires every case to succeed; failures on both sides across different cases count as both fail. “llmux-only” means the second gateway; “omniroute-only” means the first.</p><h2>Summary</h2><div class=\"strip\" aria-label=\"Verdict counts\">");
    for v in [
        "match",
        "llmux-only fail",
        "omniroute-only fail",
        "both fail",
    ] {
        html += &badge(
            v,
            Some(report["summary"]["verdicts"][v].as_u64().unwrap_or(0) as usize),
        );
    }
    html += "</div><table aria-label=\"Summary\"><thead><tr><th>Metric</th>";
    for g in &gateways {
        html += &format!("<th>{}</th>", escape(g));
    }
    html += "</tr></thead><tbody>";
    for (label, key) in [
        ("Successes / requests", "successes"),
        ("Error classes", "errors"),
        ("p50 all (ms)", "p50_ms"),
        ("p50 success (ms)", "success_p50_ms"),
        ("p50 TTFT (ms)", "success_ttft_p50_ms"),
    ] {
        html += &format!("<tr><th>{label}</th>");
        for g in &gateways {
            let summary = &report["summary"]["gateways"][g];
            let value = if key == "successes" {
                format!(
                    "{} / {}",
                    display(&summary[key]),
                    display(&summary["requests"])
                )
            } else if key == "errors" {
                let s = summary[key]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, v)| format!("{}: {}", escape(k), display(v)))
                    .collect::<Vec<_>>()
                    .join(", ");
                if s.is_empty() {
                    "—".into()
                } else {
                    s
                }
            } else {
                display(&summary[key])
            };
            html += &format!("<td>{value}</td>");
        }
        html += "</tr>";
    }
    html += "</tbody></table><h2>Models</h2><table aria-label=\"Models\"><caption>HTTP ";
    html += &escape(&gateways.join(" / "));
    html+="; colors indicate success, including payload and stream validation.</caption><thead><tr><th>Model</th><th>Chat</th><th>Stream</th><th>Responses</th><th>Verdict</th></tr></thead><tbody>";
    for model in report["models"].as_array().into_iter().flatten() {
        html += &format!("<tr><th>{}</th>", display(&model["model"]));
        for kind in ["chat", "stream", "responses"] {
            if let Some(case) = model["cases"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|c| c["kind"] == kind)
            {
                html += &format!(
                    "<td class=\"{}\">{}</td>",
                    if case["verdict"] == "match" {
                        "ok"
                    } else {
                        "fail"
                    },
                    gateways
                        .iter()
                        .map(|g| display(&case["results"][g]["status"]))
                        .collect::<Vec<_>>()
                        .join(" / ")
                );
            } else {
                html += "<td>—</td>";
            }
        }
        html += &format!(
            "<td>{}</td></tr>",
            badge(model["verdict"].as_str().unwrap_or("both fail"), None)
        );
    }
    html += "</tbody></table><h2>Run details</h2><dl>";
    for (key, label) in [
        ("completed_at", "Completed (UTC)"),
        ("timeout_s", "Timeout (seconds)"),
        ("total_http_requests_sent", "HTTP requests sent"),
        ("compared_requests", "Compared requests"),
        ("superseded_requests", "Superseded requests"),
        ("max_requests", "Request cap"),
        ("ordering", "Order"),
        ("note", "Note"),
    ] {
        if !report["run"][key].is_null() {
            html += &format!("<dt>{label}</dt><dd>{}</dd>", display(&report["run"][key]));
        }
    }
    html+="</dl><p>p50 TTFT uses successful streams only. Fallback unknown ≠ false; routing headers are evidence, not a reconstructed attempt trace. Error classes are heuristics, not proof of error provenance.</p><h2>Request details</h2><table aria-label=\"Request details\"><thead><tr><th>Model / case</th>";
    for g in &gateways {
        html += &format!("<th>{}</th>", escape(g));
    }
    html += "<th>Verdict</th></tr></thead><tbody>";
    for model in report["models"].as_array().into_iter().flatten() {
        html += &format!(
            "<tr><th colspan=\"3\">{}</th><th>{}</th></tr>",
            display(&model["model"]),
            display(&model["verdict"])
        );
        for case in model["cases"].as_array().into_iter().flatten() {
            html += &format!("<tr><td>{}</td>", display(&case["kind"]));
            for (index, g) in gateways.iter().enumerate() {
                let r = &case["results"][g];
                let mut fields = vec![
                    format!("HTTP {}", display(&r["status"])),
                    display(&r["error_class"]),
                    format!("{} ms", display(&r["latency_ms"])),
                ];
                if !r["ttft_ms"].is_null() {
                    fields.push(format!("TTFT {} ms", display(&r["ttft_ms"])));
                }
                if case["kind"] == "stream" {
                    fields.push(format!(
                        "DONE {}",
                        if r["done"] == true { "✓" } else { "✗" }
                    ));
                }
                if index == 1 {
                    if let Some(attempts) = r["attempts"].as_u64() {
                        fields.push(format!(
                            "{attempts} attempt{}{}",
                            if attempts == 1 { "" } else { "s" },
                            if attempts > 1 { " (fallback)" } else { "" }
                        ));
                    }
                }
                html += &format!(
                    "<td class=\"{}\">{}",
                    if r["error_class"] == "ok" {
                        "ok"
                    } else {
                        "fail"
                    },
                    fields.join(" · ")
                );
                if truthy(&r["deployment"]) {
                    html += &format!("<small>Deployment {}</small>", display(&r["deployment"]));
                }
                if truthy(&r["error"]) {
                    html += &format!("<small>{}</small>", display(&r["error"]));
                }
                if truthy(&r["routing_headers"]) {
                    html += &format!(
                        "<details><summary>Routing headers</summary><pre>{}</pre></details>",
                        escape(
                            &serde_json::to_string_pretty(&r["routing_headers"])
                                .unwrap_or_default()
                        )
                    );
                }
                html += "</td>";
            }
            html += &format!(
                "<td>{}<small>Same error class: {}</small></td></tr>",
                badge(case["verdict"].as_str().unwrap_or("both fail"), None),
                if case["same_error_class"] == true {
                    "yes"
                } else {
                    "no"
                }
            );
        }
    }
    if report["models"].as_array().is_none_or(Vec::is_empty) {
        html += "<tr><td colspan=\"4\">No results</td></tr>";
    }
    html += "</tbody></table></html>";
    html
}

fn error_fields(data: &Value) -> (Option<String>, Option<String>) {
    let Some(error) = data.get("error").filter(|e| !e.is_null()) else {
        return (None, None);
    };
    if error.is_object() {
        let code = error
            .get("code")
            .filter(|v| truthy(v))
            .or_else(|| error.get("type").filter(|v| truthy(v)));
        (
            Some(
                code.map(raw_string)
                    .unwrap_or_else(|| "upstream_error".into()),
            ),
            Some(
                error
                    .get("message")
                    .map(raw_string)
                    .unwrap_or_else(|| "error".into()),
            ),
        )
    } else {
        (Some("upstream_error".into()), Some(raw_string(error)))
    }
}
fn raw_string(v: &Value) -> String {
    v.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| v.to_string())
}

#[derive(Default)]
struct Sse {
    pending: Vec<u8>,
    event: Vec<Vec<u8>>,
    first_line: bool,
    skip_lf: bool,
    code: Option<String>,
    message: Option<String>,
}
impl Sse {
    fn feed(&mut self, chunk: &[u8], record: &mut Value, start: Instant) -> Result<()> {
        for byte in chunk {
            if self.skip_lf && *byte == 10 {
                self.skip_lf = false;
                continue;
            }
            self.skip_lf = *byte == 13;
            if ![10, 13].contains(byte) {
                self.pending.push(*byte);
                if self.pending.len() > MAX_EVENT {
                    return Err("SSE line exceeds limit".into());
                }
                continue;
            }
            let mut line = std::mem::take(&mut self.pending);
            if self.first_line {
                if line.starts_with(b"\xef\xbb\xbf") {
                    line.drain(..3);
                }
                self.first_line = false;
            }
            if line.is_empty() {
                self.consume(record, start)?;
            } else if let Some(data) = line.strip_prefix(b"data:") {
                self.event
                    .push(data.iter().skip_while(|b| **b == b' ').copied().collect());
            }
            if self.event.iter().map(Vec::len).sum::<usize>() > MAX_EVENT {
                return Err("SSE event exceeds limit".into());
            }
        }
        Ok(())
    }
    fn consume(&mut self, record: &mut Value, start: Instant) -> Result<()> {
        if self.event.is_empty() {
            return Ok(());
        }
        let data = self.event.join(&b'\n');
        self.event.clear();
        let text = String::from_utf8_lossy(&data);
        if text.trim() == "[DONE]" {
            record["done"] = json!(true);
            return Ok(());
        }
        let obj: Value = serde_json::from_str(&text)?;
        let (code, message) = error_fields(&obj);
        if code.is_some() {
            self.code = code;
            self.message = message;
        }
        if !obj.is_object() {
            return Err("invalid SSE payload".into());
        }
        if let Some(choices) = obj.get("choices") {
            for choice in choices.as_array().ok_or("invalid SSE choices")? {
                if !choice.is_object() {
                    return Err("invalid SSE choice".into());
                }
                if let Some(delta) = choice.get("delta") {
                    if !delta.is_object() {
                        return Err("invalid SSE delta".into());
                    }
                    if truthy(&delta["content"]) && record["ttft_ms"].is_null() {
                        record["ttft_ms"] = json!(round(start.elapsed().as_secs_f64() * 1000.0));
                    }
                }
            }
        }
        Ok(())
    }
}
async fn send(gateway: &Value, case: &Value, timeout: f64, secrets: &[String]) -> Value {
    let start = Instant::now();
    let streaming = case["kind"] == "stream";
    let mut record = json!({"gateway":gateway["name"],"model":case["model"],"kind":case["kind"],"status":null,"error_class":null,"error":null,"latency_ms":null,"ttft_ms":null,"done":if streaming {json!(false)} else {Value::Null},"deployment":null,"attempts":null,"fallback":null,"routing_headers":{},"started_at":chrono::Utc::now().to_rfc3339()});
    let mut parser = Sse {
        first_line: true,
        ..Sse::default()
    };
    let mut transport = None;
    let mut message = None;
    let operation = async {
        let url = format!(
            "{}{}",
            gateway["base_url"]
                .as_str()
                .ok_or("missing base URL")?
                .trim_end_matches('/'),
            case["path"].as_str().ok_or("missing path")?
        );
        validate_url(&url)?;
        let client = crate::http_client::client(&url)?;
        let request = Request::post(url)
            .header(
                "authorization",
                format!("Bearer {}", gateway["key"].as_str().ok_or("missing key")?),
            )
            .header("content-type", "application/json")
            .header("accept-encoding", "identity")
            .body(Full::new(Bytes::from(
                case["body"].as_str().ok_or("missing body")?.to_owned(),
            )))?;
        let response = client.request(request).await?;
        let status = response.status().as_u16();
        record["status"] = json!(status);
        let headers: Vec<_> = response
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str(), v)))
            .collect();
        record["routing_headers"] = routing_headers(&headers, secrets);
        record["deployment"] = record["routing_headers"]["x-llmux-deployment"].clone();
        if let Some(attempts) = record["routing_headers"]["x-llmux-attempts"]
            .as_str()
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse::<u64>().ok())
        {
            record["attempts"] = json!(attempts);
            record["fallback"] = json!(attempts > 1);
        }
        let sized = response.headers().contains_key("content-length");
        let sse = streaming && (200..300).contains(&status);
        let mut body = response.into_body();
        let mut raw = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame?;
            if let Some(chunk) = frame.data_ref() {
                if sse {
                    parser.feed(chunk, &mut record, start)?;
                    if record["done"] == true && !sized {
                        break;
                    }
                } else {
                    raw.extend_from_slice(chunk);
                    if raw.len() > MAX_BODY {
                        return Err("response exceeds limit".into());
                    }
                }
            }
        }
        if sse {
            if record["done"] != true && parser.code.is_none() {
                transport = Some("connection");
                message = Some("stream ended without [DONE]".to_owned());
            }
        } else {
            let data: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
            let (code, msg) = error_fields(&data);
            parser.code = code;
            message = msg;
            if !(200..300).contains(&status) && message.is_none() {
                message = Some(format!("HTTP {status} (non-JSON error body omitted)"));
            } else if parser.code.is_none() && (200..300).contains(&status) {
                let field = if case["kind"] == "responses" {
                    "output"
                } else {
                    "choices"
                };
                if data[field]
                    .as_array()
                    .is_none_or(|a| a.is_empty() || a.iter().any(|v| !v.is_object()))
                {
                    transport = Some("connection");
                    message = Some("invalid success payload (body omitted)".into());
                }
            }
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    };
    match tokio::time::timeout(Duration::from_secs_f64(timeout), operation).await {
        Err(_) => {
            transport = Some("timeout");
            message = Some("request deadline exceeded".into());
        }
        Ok(Err(_)) => {
            transport = Some("connection");
            message = Some("connection or HTTP framing failure".into());
        }
        Ok(Ok(())) => {}
    }
    let status = record["status"].as_u64().map(|s| s as u16);
    let class = classify(status, parser.code.as_deref(), transport);
    record["latency_ms"] = json!(round(start.elapsed().as_secs_f64() * 1000.0));
    record["error_class"] = json!(class);
    if class != "ok" {
        record["error"] = json!(redact(
            &message.or(parser.message).unwrap_or_else(|| format!(
                "HTTP {}",
                status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "None".into())
            )),
            secrets
        ));
    }
    record
}

fn private_write(path: &std::path::Path, text: &str, exclusive: bool) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = fs::OpenOptions::new();
    options.write(true).mode(0o600);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}
struct Inputs {
    gateways: Vec<Value>,
    cases: Vec<Value>,
    plan: Vec<(usize, usize)>,
    timeout: f64,
    cap: usize,
    output: PathBuf,
}
impl Inputs {
    fn parse(flags: &[String]) -> Result<Self> {
        let mut args = flags.iter();
        let mut gateways = Vec::new();
        let mut models = Vec::new();
        let mut source = None;
        let mut top = 25;
        let mut responses = 0;
        let mut timeout: f64 = 45.0;
        let mut cap = 80;
        let mut output = None;
        while let Some(flag) = args.next() {
            let value = args.next().ok_or("missing option value")?;
            match flag.as_str() {
                "--gateway" => {
                    let url = args.next().ok_or("missing gateway URL")?;
                    let reference = args.next().ok_or("missing key ref")?;
                    validate_url(url)?;
                    gateways.push(json!({"name":value,"base_url":url,"key":read_key(reference)?}));
                }
                "--model" => models.push(Value::String(value.clone())),
                "--models-file" | "--coverage" => {
                    if source.is_some() {
                        return Err("conflicting model source".into());
                    }
                    source = Some((flag.as_str(), value));
                }
                "--top" => top = value.parse()?,
                "--responses" => responses = value.parse()?,
                "--timeout" => timeout = value.parse()?,
                "--max-requests" => cap = value.parse()?,
                "--output" => output = Some(PathBuf::from(value)),
                _ => return Err("unknown option".into()),
            }
        }
        if !timeout.is_finite() || timeout <= 0.0 {
            return Err("timeout must be positive and finite".into());
        }
        if let Some((kind, path)) = source {
            if !models.is_empty() {
                return Err("conflicting model source".into());
            }
            let text = fs::read_to_string(path)?;
            models = if kind == "--coverage" {
                coverage_models(&serde_json::from_str(&text)?, top)?
                    .into_iter()
                    .map(Value::String)
                    .collect()
            } else if text.trim_start().starts_with('[') {
                serde_json::from_str::<Value>(&text)?
                    .as_array()
                    .ok_or("models JSON must be an array")?
                    .clone()
            } else {
                text.lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| Value::String(s.into()))
                    .collect()
            };
        }
        let cases = generate_cases(&Value::Array(models), responses)?;
        let names: Vec<_> = gateways
            .iter()
            .map(|g| g["name"].as_str().unwrap_or_default().to_owned())
            .collect();
        let plan = make_plan(&cases, &names, cap)?;
        let output = output.ok_or("missing --output")?;
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(&output)?;
        Ok(Self {
            gateways,
            cases,
            plan,
            timeout,
            cap,
            output,
        })
    }
}
pub async fn run(args: &[String]) -> Result<()> {
    let inputs=Inputs::parse(args).map_err(|_|"Invalid inputs, over-budget plan, unreadable key/model file, or output already exists. Nothing sent.")?;
    for gateway in &inputs.gateways {
        crate::http_client::client(gateway["base_url"].as_str().ok_or("missing base URL")?)?;
    }
    let secrets: Vec<_> = inputs
        .gateways
        .iter()
        .filter_map(|g| g["key"].as_str().map(str::to_owned))
        .collect();
    let names: Vec<_> = inputs
        .gateways
        .iter()
        .filter_map(|g| g["name"].as_str().map(str::to_owned))
        .collect();
    let mut rows = Vec::new();
    println!(
        "Sending {} HTTP requests ({} paired cases), cap {}.",
        inputs.plan.len(),
        inputs.cases.len(),
        inputs.cap
    );
    for (i, (case, gateway)) in inputs.plan.iter().enumerate() {
        rows.push(
            send(
                &inputs.gateways[*gateway],
                &inputs.cases[*case],
                inputs.timeout,
                &secrets,
            )
            .await,
        );
        private_write(
            &inputs.output.join("partial.json"),
            &serde_json::to_string_pretty(&rows)?,
            false,
        )?;
        println!("{}/{} completed", i + 1, inputs.plan.len());
    }
    let mut report = aggregate(&rows, &names)?;
    report["run"] = json!({"completed_at":chrono::Utc::now().to_rfc3339(),"timeout_s":inputs.timeout,"max_requests":inputs.cap,"ordering":"alternating pairs; sequential; no retries","latency_note":"one sample per gateway/case; includes errors; no speedup inference"});
    for (file, mut text) in [
        ("results.json", serde_json::to_string_pretty(&report)?),
        ("report.html", render_html(&report)),
    ] {
        for secret in &secrets {
            text = text.replace(secret, "[REDACTED]");
        }
        private_write(&inputs.output.join(file), &text, true)?;
    }
    fs::remove_file(inputs.output.join("partial.json"))?;
    println!("{}", serde_json::to_string_pretty(&report["summary"])?);
    Ok(())
}

#[cfg(test)]
mod tests;
