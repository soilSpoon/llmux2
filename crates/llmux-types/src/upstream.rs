//! Shared upstream-endpoint rules: HTTPS everywhere, plain HTTP only to loopback.

use bytes::Bytes;
use http::Uri;
use http_body_util::Full;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use std::net::IpAddr;

pub type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// Whether a URI host names the local machine: `localhost` or a loopback IP.
pub fn host_is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether the URI is HTTPS, or plain HTTP to a loopback host (tests only).
pub fn is_https_or_loopback_http(uri: &Uri) -> bool {
    match uri.scheme_str() {
        Some("https") => uri.host().is_some(),
        Some("http") => uri.host().is_some_and(host_is_loopback),
        _ => false,
    }
}

/// One TLS-capable connector for every upstream client.
pub fn https_connector() -> HttpsConnector<HttpConnector> {
    HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build()
}

/// The shared upstream HTTP client (no retry of canceled requests).
pub fn http_client() -> HttpClient {
    Client::builder(TokioExecutor::new())
        .retry_canceled_requests(false)
        .build(https_connector())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_and_https_rules() {
        for host in ["localhost", "LOCALHOST", "127.0.0.1", "::1", "[::1]"] {
            assert!(host_is_loopback(host), "{host}");
        }
        for host in [
            "example.com",
            "8.8.8.8",
            "0.0.0.0",
            "localhost.evil.com",
            "",
        ] {
            assert!(!host_is_loopback(host), "{host}");
        }
        for uri in [
            "https://api.example.com/v1",
            "http://127.0.0.1:8080/v1",
            "http://localhost:8080",
            "http://[::1]:8080",
        ] {
            assert!(is_https_or_loopback_http(&uri.parse().unwrap()), "{uri}");
        }
        for uri in [
            "http://example.com",
            "ftp://127.0.0.1",
            "/relative/path",
            "http://localhost.evil.com",
        ] {
            assert!(!is_https_or_loopback_http(&uri.parse().unwrap()), "{uri}");
        }
    }
}
