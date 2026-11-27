//! `llmux` — thin CLI client for the llmux2 admin API.

use bytes::Bytes;
use http::{header, Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use std::process::ExitCode;

const USAGE: &str = "usage: llmux [--url URL] [--key KEY] <command>

Commands:
  status | deployments | credentials | models | reload
  enable <credential-id> | disable <credential-id>

Target: --url/--key or LLMUX_ADMIN_URL/LLMUX_ADMIN_KEY (default http://127.0.0.1:8080).";

struct Args {
    url: String,
    key: Option<String>,
    command: String,
    arg: Option<String>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args {
        url: std::env::var("LLMUX_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
        key: std::env::var("LLMUX_ADMIN_KEY").ok(),
        command: String::new(),
        arg: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--url" => parsed.url = args.next().ok_or("--url needs a value")?,
            "--key" => parsed.key = Some(args.next().ok_or("--key needs a value")?),
            flag if flag.starts_with("--") => {
                // Report the flag name only: `--key=SECRET` must not echo the key.
                return Err(format!(
                    "unknown flag {}",
                    flag.split('=').next().unwrap_or(flag)
                ));
            }
            command if parsed.command.is_empty() => parsed.command = command.to_owned(),
            arg => {
                if parsed.arg.replace(arg.to_owned()).is_some() {
                    return Err("too many arguments".into());
                }
            }
        }
    }
    if parsed.command.is_empty() {
        return Err("missing command".into());
    }
    Ok(parsed)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(body) => {
            println!("{body}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("llmux: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<String, String> {
    let args = parse(std::env::args().skip(1)).map_err(|err| format!("{err}\n{USAGE}"))?;
    let (method, path) = match (args.command.as_str(), args.arg.as_deref()) {
        ("status", None) => (Method::GET, "/admin/status".into()),
        ("deployments", None) => (Method::GET, "/admin/deployments".into()),
        ("credentials", None) => (Method::GET, "/admin/credentials".into()),
        ("models", None) => (Method::GET, "/admin/models".into()),
        ("reload", None) => (Method::POST, "/admin/reload".into()),
        ("enable", Some(id)) | ("disable", Some(id)) => (
            Method::POST,
            format!("/admin/credentials/{}/disabled", urlencoding(id)),
        ),
        (command, _) => return Err(format!("unknown command `{command}`\n{USAGE}")),
    };
    let body = match args.command.as_str() {
        "enable" => serde_json::json!({"disabled": false}),
        "disable" => serde_json::json!({"disabled": true}),
        _ => serde_json::json!({}),
    };
    // Percent-encode credential ids: config accepts `/`, `?` and `%` in ids,
    // and those must not change the route or resolve to another credential.
    fn urlencoding(id: &str) -> String {
        let mut encoded = String::new();
        for byte in id.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                encoded.push(byte as char);
            } else {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        }
        encoded
    }
    let parsed_url = url::Url::parse(&args.url).map_err(|err| format!("bad --url: {err}"))?;
    if parsed_url.scheme() == "http" && !loopback_url(&parsed_url) && args.key.is_some() {
        return Err(
            "refusing to send the admin key over plaintext http to a non-loopback host".into(),
        );
    }
    let connector = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .build();
    let client = Client::builder(TokioExecutor::new()).build(connector);
    let mut request = Request::builder()
        .method(method)
        .uri(format!("{}{}", args.url.trim_end_matches('/'), path))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = &args.key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.request(
            request
                .body(Full::new(Bytes::from(body.to_string())))
                .map_err(|err| err.to_string())?,
        ),
    )
    .await
    .map_err(|_| "request timed out after 30s".to_string())?
    .map_err(|err| format!("request failed: {err}"))?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|err| format!("reading response failed: {err}"))?
        .to_bytes();
    if status != StatusCode::OK {
        let detail = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|body| body["error"]["message"].as_str().map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
        return Err(format!("{status}: {detail}"));
    }
    let pretty: serde_json::Value =
        serde_json::from_slice(&body).map_err(|err| format!("invalid JSON response: {err}"))?;
    Ok(serde_json::to_string_pretty(&pretty).unwrap())
}

// 127.0.0.0/8 and ::1 are all loopback, not just 127.0.0.1.
fn loopback_url(parsed_url: &url::Url) -> bool {
    match parsed_url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(domain)) => domain == "localhost",
        None => false,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn plaintext_loopback_classification_covers_the_whole_ipv4_range() {
        for url in [
            "http://127.0.0.1:8080",
            "http://127.0.0.2:8080",
            "http://127.99.100.101:8080",
            "http://[::1]:8080",
            "http://localhost:8080",
        ] {
            assert!(
                super::loopback_url(&url::Url::parse(url).unwrap()),
                "{url} must be treated as loopback"
            );
        }

        for url in ["http://example.com", "http://192.0.2.1"] {
            assert!(
                !super::loopback_url(&url::Url::parse(url).unwrap()),
                "{url} must not be treated as loopback"
            );
        }
    }
}
