//! Free-model discovery: find new free-tier upstream ids on connected
//! providers and verify them with a one-token probe before anyone wires
//! them into routing. Adapters per provider extract candidate ids from
//! whatever free signal the provider exposes; the probe is the ground
//! truth (`usage.cost == 0` means the key pays nothing).
//!
//! Never probe an id a provider marks paid: billed attempts cost real
//! money on funded accounts. Probe only candidates an adapter surfaced
//! through a free signal.

use bytes::Bytes;
use http::{Request, StatusCode, Uri};
use http_body_util::{BodyExt, Full, Limited};
use llmux_types::{http_client, is_https_or_loopback_http, HttpClient};
use serde_json::{json, Value};
use std::time::Duration;

pub const EXPERLABS_PAGE: &str = "https://platform.experientiallabs.ai/models";
pub const EXPERLABS_API: &str = "https://api.experientiallabs.ai/v1";

/// A stalled page or probe endpoint must not wedge the whole scan.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const PAGE_TIMEOUT: Duration = Duration::from_secs(20);
/// Body caps: a streaming or oversized response must not exhaust memory.
const MAX_PROBE_BODY: usize = 64 * 1024;
const MAX_PAGE_BODY: usize = 1024 * 1024;

pub fn client() -> Result<HttpClient, String> {
    Ok(http_client())
}

pub struct Candidate {
    pub id: String,
    pub evidence: String,
}

#[derive(Debug)]
pub enum ProbeVerdict {
    /// `usage.cost == 0` — the key pays nothing for this model.
    Free,
    /// A positive cost or an explicit billing error — never wire as free.
    Billed,
    /// The provider no longer serves the model (404 / model_not_allowed):
    /// also the signal that an existing free leg got terminated.
    Unavailable(String),
    /// Anything else — network, non-chat endpoints, missing cost field.
    /// Reported but never auto-adopted.
    Unknown(String),
}

impl std::fmt::Display for ProbeVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Free => write!(f, "free"),
            Self::Billed => write!(f, "billed"),
            Self::Unavailable(why) => write!(f, "unavailable({why})"),
            Self::Unknown(why) => write!(f, "unknown({why})"),
        }
    }
}

const PROMO_MARKER: &str = "Free as ";

fn id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':')
}

/// Candidate upstream ids from Experiential's public models page. The
/// gateway `/v1/models` never lists `:free` ids, so the promo marker on the
/// public page is the discovery signal.
pub fn experlabs_promo_ids(html: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for part in html.split(PROMO_MARKER).skip(1) {
        let id: String = part.chars().take_while(|&c| id_char(c)).collect();
        if !id.is_empty() && !out.iter().any(|c| c.id == id) {
            out.push(Candidate {
                id,
                evidence: "experlabs page promo".to_string(),
            });
        }
    }
    out
}

/// One-token chat probe: classify whether the key pays for `model`.
/// Requires `base_url` ending before `/chat/completions` (e.g. `.../v1`).
pub async fn probe_model(
    client: &HttpClient,
    base_url: &str,
    key: &str,
    model: &str,
) -> ProbeVerdict {
    probe_model_with_deadline(client, base_url, key, model, PROBE_TIMEOUT).await
}

/// `probe_model` with an explicit deadline, so tests can use a short one.
pub async fn probe_model_with_deadline(
    client: &HttpClient,
    base_url: &str,
    key: &str,
    model: &str,
    timeout: Duration,
) -> ProbeVerdict {
    // The key rides the Authorization header: plaintext HTTP to a
    // non-loopback host would leak it — the same https-or-loopback policy
    // as upstream_uri.
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    match url.parse::<Uri>() {
        Ok(uri) if is_https_or_loopback_http(&uri) => {}
        _ => {
            return ProbeVerdict::Unknown(format!(
                "probe base must be https or loopback http: {base_url}"
            ));
        }
    }
    let payload = json!({
        "model": model,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1,
        "stream": false,
    });
    let request = match Request::post(&url)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(payload.to_string())))
    {
        Ok(request) => request,
        Err(e) => return ProbeVerdict::Unknown(e.to_string()),
    };
    let exchange = async {
        let response = client.request(request).await.map_err(|e| e.to_string())?;
        let status = response.status();
        let bytes = Limited::new(response.into_body(), MAX_PROBE_BODY)
            .collect()
            .await
            .map(|c| c.to_bytes())
            .map_err(|e| e.to_string())?;
        Ok::<_, String>((status, bytes))
    };
    match tokio::time::timeout(timeout, exchange).await {
        Ok(Ok((status, bytes))) => classify(status, &bytes),
        Ok(Err(e)) => ProbeVerdict::Unknown(e),
        Err(_) => ProbeVerdict::Unknown(format!("probe timed out after {timeout:?}")),
    }
}

/// Error text for classification: providers put the machine code in
/// different fields — `code`, `type`, or only `message` — and a numeric
/// `code` must not hide a `type: insufficient_quota`.
fn error_text(body: &Value) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let error = &body["error"];
    for field in [
        &error["code"],
        &error["type"],
        &error["message"],
        &body["code"],
        &body["type"],
        &body["message"],
    ] {
        if let Some(s) = field.as_str() {
            parts.push(s);
        }
    }
    parts.join(" ").to_ascii_lowercase()
}

fn classify(status: StatusCode, bytes: &[u8]) -> ProbeVerdict {
    let body = serde_json::from_slice::<Value>(bytes).ok();
    if status.is_success() {
        return match body
            .as_ref()
            .and_then(|v| v.pointer("/usage/cost"))
            .and_then(Value::as_f64)
        {
            Some(cost) if cost <= 0.0 => ProbeVerdict::Free,
            Some(_) => ProbeVerdict::Billed,
            None => ProbeVerdict::Unknown(format!("{status}: no usage.cost")),
        };
    }
    let code = body.as_ref().map(error_text).unwrap_or_default();
    // Quota/rate-limit class first: a shared free tier's daily cap can
    // surface as 429 or a code like `insufficient_quota` — a parked leg,
    // never a billing verdict. Checking it before the billed heuristics
    // keeps "insufficient_quota" out of the removal path (hard rule).
    if status == StatusCode::TOO_MANY_REQUESTS
        || code.contains("quota")
        || code.contains("rate_limit")
        || code.contains("rate-limit")
        || code.contains("ratelimit")
    {
        ProbeVerdict::Unknown(format!("{status}: {code}"))
    } else if status == StatusCode::PAYMENT_REQUIRED
        || code.contains("insufficient")
        || code.contains("credit")
        || code.contains("billing")
        || code.contains("payment")
    {
        ProbeVerdict::Billed
    } else if status == StatusCode::NOT_FOUND
        || code.contains("model_not_allowed")
        || code.contains("not_found")
        || code.contains("does not exist")
    {
        ProbeVerdict::Unavailable(format!("{status}: {code}"))
    } else {
        ProbeVerdict::Unknown(format!("{status}: {code}"))
    }
}

pub struct ScanRow {
    pub id: String,
    pub evidence: String,
    /// Whether the id is already in the provider's reviewed free map.
    pub listed: bool,
    /// `None` when no key was given — candidates stay unverified.
    pub verdict: Option<ProbeVerdict>,
}

async fn fetch_text(client: &HttpClient, url: &str) -> Result<String, String> {
    let fetch = async {
        let request = Request::get(url)
            .header("user-agent", "llmux-free-scan")
            .body(Full::new(Bytes::new()))
            .map_err(|e| e.to_string())?;
        let response = client.request(request).await.map_err(|e| e.to_string())?;
        let status = response.status();
        let bytes = Limited::new(response.into_body(), MAX_PAGE_BODY)
            .collect()
            .await
            .map(|c| c.to_bytes())
            .map_err(|e| e.to_string())?;
        Ok::<_, String>((status, bytes))
    };
    let (status, bytes) = tokio::time::timeout(PAGE_TIMEOUT, fetch)
        .await
        .map_err(|_| format!("GET {url}: timed out after {PAGE_TIMEOUT:?}"))??;
    if !status.is_success() {
        return Err(format!("GET {url}: {status}"));
    }
    String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())
}

/// One provider's discovery plan: where candidates come from and where
/// probes go. New providers add a variant plus an adapter function.
pub enum ScanPlan {
    Experlabs { page: String, base: String },
}

impl ScanPlan {
    pub async fn run(
        &self,
        client: &HttpClient,
        key: Option<&str>,
        known: &[String],
    ) -> Result<Vec<ScanRow>, String> {
        match self {
            Self::Experlabs { page, base } => scan_experlabs(client, page, base, key, known).await,
        }
    }
}

/// Scan one provider for free-model candidates and verify each against
/// `key`. `known` marks ids already in the provider's reviewed free map.
pub async fn scan_experlabs(
    client: &HttpClient,
    page_url: &str,
    probe_base: &str,
    key: Option<&str>,
    known: &[String],
) -> Result<Vec<ScanRow>, String> {
    let html = fetch_text(client, page_url).await?;
    let mut rows = Vec::new();
    for candidate in experlabs_promo_ids(&html) {
        let verdict = match key {
            Some(key) => Some(probe_model(client, probe_base, key, &candidate.id).await),
            None => None,
        };
        rows.push(ScanRow {
            listed: known.iter().any(|k| k == &candidate.id),
            id: candidate.id,
            evidence: candidate.evidence,
            verdict,
        });
    }
    Ok(rows)
}
