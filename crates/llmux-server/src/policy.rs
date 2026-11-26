//! End-user hashing and inbound image checks. One place, before any upstream call.
//!
//! The raw identifier is never logged and never copied into an error string.

use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use llmux_config::{KeyPolicy, RemoteImages, ServerConfig};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub(crate) const END_USER_HEADER: HeaderName = HeaderName::from_static("x-llmux-end-user");
pub(crate) const REMOTE_IMAGE_MESSAGE: &str =
    "remote image URLs are not accepted; send a data: URL";

/// `hex(sha256(salt || value))` over the UTF-8 bytes, with no separator.
pub(crate) fn hash_end_user(salt: &str, value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(value.as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

#[derive(Debug)]
pub(crate) struct EndUserError;

impl EndUserError {
    pub(crate) fn message(&self) -> &'static str {
        "end-user header must be a single valid text value"
    }
}

pub(crate) fn resolve_end_user(
    server: &ServerConfig,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Option<Arc<str>>, EndUserError> {
    let Some(name) = &server.end_user_header else {
        return Ok(None);
    };
    let salt = server
        .end_user_salt
        .as_deref()
        .expect("end_user_salt is required when end_user_header is set");
    match header_identity(headers, name) {
        HeaderRead::Value(value) => return Ok(Some(Arc::from(hash_end_user(salt, &value)))),
        HeaderRead::Absent => {}
        HeaderRead::Invalid => return Err(EndUserError),
    }
    if !server.end_user_from_body {
        return Ok(None);
    }
    let Ok(payload) = serde_json::from_slice::<Value>(body) else {
        return Ok(None);
    };
    let Some(user) = payload.get("user").and_then(Value::as_str) else {
        return Ok(None);
    };
    if user.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(Arc::from(hash_end_user(salt, user))))
}

enum HeaderRead {
    Value(String),
    Absent,
    Invalid,
}

fn header_identity(headers: &HeaderMap, name: &HeaderName) -> HeaderRead {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return HeaderRead::Absent;
    };
    if values.next().is_some() {
        return HeaderRead::Invalid;
    }
    let Ok(text) = value.to_str() else {
        return HeaderRead::Invalid;
    };
    if text.trim().is_empty() {
        HeaderRead::Absent
    } else {
        HeaderRead::Value(text.to_owned())
    }
}

pub(crate) fn attach_end_user(
    mut response: axum::response::Response,
    end_user: Option<&str>,
) -> axum::response::Response {
    if let Some(hash) = end_user {
        if let Ok(value) = HeaderValue::from_str(hash) {
            response.headers_mut().insert(END_USER_HEADER, value);
        }
    }
    response
}

#[derive(Debug)]
pub(crate) enum ImagePolicyError {
    Remote,
    TooLarge,
    InvalidData,
}

impl ImagePolicyError {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Remote | Self::InvalidData => StatusCode::BAD_REQUEST,
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Remote => "remote_image_url",
            Self::TooLarge => "payload_too_large",
            Self::InvalidData => "invalid_image_data",
        }
    }

    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::Remote => REMOTE_IMAGE_MESSAGE,
            Self::TooLarge => "image exceeds max_image_bytes; send a smaller data: URL",
            Self::InvalidData => "invalid image data URL",
        }
    }
}

/// Image inputs anywhere in the body, for the per-request log. Matches the
/// shapes `walk` checks: `image_url` fields (OpenAI chat/Responses) and
/// Anthropic `type: "image"` blocks.
pub(crate) fn count_images(body: &[u8]) -> u32 {
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return 0;
    };
    if let Some(request) = value.as_object_mut() {
        for field in ["tools", "functions", "response_format"] {
            request.remove(field);
        }
        if let Some(text) = request.get_mut("text").and_then(Value::as_object_mut) {
            text.remove("format");
        }
    }
    count_walk(&value)
}

fn count_walk(value: &Value) -> u32 {
    match value {
        Value::Array(items) => items.iter().map(count_walk).sum(),
        Value::Object(map) => {
            let mut count = 0;
            if map.contains_key("image_url")
                || map.get("type").and_then(Value::as_str) == Some("image")
            {
                count += 1;
            }
            count + map.values().map(count_walk).sum::<u32>()
        }
        _ => 0,
    }
}

pub(crate) fn check_images(body: &[u8], server: &ServerConfig) -> Result<(), ImagePolicyError> {
    if server.remote_images == RemoteImages::Allow && server.max_image_bytes.is_none() {
        return Ok(());
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return Ok(());
    };
    if let Some(request) = value.as_object_mut() {
        for field in ["tools", "functions", "response_format"] {
            request.remove(field);
        }
        if let Some(text) = request.get_mut("text").and_then(Value::as_object_mut) {
            text.remove("format");
        }
    }
    walk(&value, server)
}

fn walk(value: &Value, server: &ServerConfig) -> Result<(), ImagePolicyError> {
    match value {
        Value::Array(items) => {
            for item in items {
                walk(item, server)?;
            }
        }
        Value::Object(map) => {
            check_object(map, server)?;
            for item in map.values() {
                walk(item, server)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_object(map: &Map<String, Value>, server: &ServerConfig) -> Result<(), ImagePolicyError> {
    if let Some(image_url) = map.get("image_url") {
        let url = match image_url {
            Value::String(url) => Some(url.as_str()),
            Value::Object(inner) => inner.get("url").and_then(Value::as_str),
            _ => None,
        };
        if let Some(url) = url {
            check_url(url, server)?;
        }
    }
    if map.get("type").and_then(Value::as_str) == Some("image") {
        if let Some(source) = map.get("source").and_then(Value::as_object) {
            if let Some(url) = source.get("url").and_then(Value::as_str) {
                check_url(url, server)?;
            }
            if source.get("type").and_then(Value::as_str) == Some("base64") {
                if let Some(data) = source.get("data").and_then(Value::as_str) {
                    check_base64(data, server)?;
                }
            }
        }
    }
    Ok(())
}

fn check_url(url: &str, server: &ServerConfig) -> Result<(), ImagePolicyError> {
    let url = url.trim();
    if server.remote_images == RemoteImages::Reject && is_remote_http(url) {
        return Err(ImagePolicyError::Remote);
    }
    if server.max_image_bytes.is_some() && has_data_scheme(url) {
        check_data_url(url, server)?;
    }
    Ok(())
}

fn is_remote_http(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && rest.starts_with("//")
}

fn has_data_scheme(url: &str) -> bool {
    url.len() >= 5 && url[..5].eq_ignore_ascii_case("data:")
}

fn check_data_url(url: &str, server: &ServerConfig) -> Result<(), ImagePolicyError> {
    let Some(data) = base64_payload(url) else {
        return Err(ImagePolicyError::InvalidData);
    };
    check_base64(data, server)
}

fn check_base64(data: &str, server: &ServerConfig) -> Result<(), ImagePolicyError> {
    let Some(limit) = server.max_image_bytes else {
        return Ok(());
    };
    let len = decoded_len(data).map_err(|_| ImagePolicyError::InvalidData)?;
    if len > limit {
        return Err(ImagePolicyError::TooLarge);
    }
    Ok(())
}

fn base64_payload(url: &str) -> Option<&str> {
    let marker = ";base64,";
    let lower = url.to_ascii_lowercase();
    let index = lower.find(marker)?;
    Some(&url[index + marker.len()..])
}

fn decoded_len(data: &str) -> Result<u64, ()> {
    let bytes = data.as_bytes();
    if bytes.is_empty() {
        return Ok(0);
    }
    if bytes.iter().any(|byte| !is_b64(*byte)) {
        return Err(());
    }
    let pad = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if pad > 2 || bytes[..bytes.len() - pad].contains(&b'=') {
        return Err(());
    }
    if pad > 0 && !bytes.len().is_multiple_of(4) {
        return Err(());
    }
    let significant = bytes.len() - pad;
    if pad == 0 && significant % 4 == 1 {
        return Err(());
    }
    Ok((significant as u64) * 3 / 4)
}

fn is_b64(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'-' | b'_' | b'=')
}

/// Whether the global `[server] deny` globs reject `model`. The check runs
/// on the requested name for every key, before any resolution.
pub(crate) fn model_denied(deny: &[String], model: &str) -> bool {
    deny.iter().any(|pattern| glob_match(pattern, model))
}

/// Whether a restricted key may request `model`. An empty `models` list
/// allows every name; a `blocked` match always wins.
pub(crate) fn allows_model(policy: &KeyPolicy, model: &str) -> bool {
    (policy.models.is_empty()
        || policy
            .models
            .iter()
            .any(|pattern| glob_match(pattern, model)))
        && !policy
            .blocked
            .iter()
            .any(|pattern| glob_match(pattern, model))
}

/// Glob: `*` spans any run (including `/`), `?` one character, other
/// characters literal. Case-sensitive: model ids keep their case end to end.
/// The `*` arm must run before literal equality so a `*` in the text does not
/// consume the pattern's star without recording a backtrack point.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let (mut star, mut star_t) = (usize::MAX, 0);
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = p;
            star_t = t;
            p += 1;
        } else if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if star != usize::MAX {
            p = star + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use llmux_config::RemoteImages;
    use std::time::Duration;

    const SALT: &str = "salt-one";
    const RAW: &str = "user-one";
    const HASH: &str = "31ca399bfc61dac0b442a6bef7c00cd06cf9682c11b9192fb26a918b4a9130f8";

    fn server(remote: RemoteImages, max_image_bytes: Option<u64>, from_body: bool) -> ServerConfig {
        ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            client_keys: Box::new([]),
            key_policies: Box::new([]),
            deny: Box::new([]),
            request_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(1),
            drain_grace: Duration::from_millis(0),
            expose_routing_headers: false,
            dashboard_key: None,
            request_log: None,
            end_user_header: Some(HeaderName::from_static("x-client-user")),
            end_user_salt: Some(Arc::from(SALT)),
            end_user_from_body: from_body,
            remote_images: remote,
            max_image_bytes,
        }
    }

    #[test]
    fn hash_is_deterministic_and_salt_changes_it() {
        assert_eq!(hash_end_user(SALT, RAW), HASH);
        assert_eq!(hash_end_user(SALT, RAW), hash_end_user(SALT, RAW));
        assert_ne!(hash_end_user("salt-two", RAW), HASH);
        assert_ne!(hash_end_user(SALT, "user-two"), HASH);
        assert_eq!(HASH.len(), 64);
        assert!(HASH.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn raw_identifier_never_appears_in_the_hash_or_errors() {
        let raw = "raw-end-user-SHOULD-NOT-LEAK";
        let hash = hash_end_user(SALT, raw);
        assert!(!hash.contains(raw));
        assert!(!hash.contains("SHOULD"));
        assert!(!hash.contains(SALT));
        let mut headers = HeaderMap::new();
        headers.append("x-client-user", HeaderValue::from_str(raw).unwrap());
        headers.append("x-client-user", HeaderValue::from_static("other"));
        let err = resolve_end_user(&server(RemoteImages::Allow, None, true), &headers, b"{}")
            .unwrap_err();
        assert!(!err.message().contains(raw));
        assert!(!err.message().contains("SHOULD"));
        assert!(!format!("{:?}", err.message()).contains(raw));
    }

    #[test]
    fn header_wins_over_body_and_empty_header_falls_through() {
        let cfg = server(RemoteImages::Allow, None, true);
        let mut headers = HeaderMap::new();
        headers.insert("x-client-user", HeaderValue::from_static(RAW));
        let hash = resolve_end_user(&cfg, &headers, br#"{"user":"body-user"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(hash.as_ref(), HASH);
        assert!(!hash.contains("body-user"));

        headers.insert("x-client-user", HeaderValue::from_static("   "));
        let from_body = resolve_end_user(&cfg, &headers, br#"{"user":"body-user"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(
            from_body.as_ref(),
            hash_end_user(SALT, "body-user").as_str()
        );
        assert!(
            resolve_end_user(&cfg, &HeaderMap::new(), br#"{"user":"  "}"#)
                .unwrap()
                .is_none()
        );
        let header_only = server(RemoteImages::Allow, None, false);
        assert!(
            resolve_end_user(&header_only, &HeaderMap::new(), br#"{"user":"body-user"}"#)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn disabled_feature_ignores_header_and_body() {
        let mut cfg = server(RemoteImages::Allow, None, true);
        cfg.end_user_header = None;
        cfg.end_user_salt = None;
        cfg.end_user_from_body = false;
        let mut headers = HeaderMap::new();
        headers.insert("x-client-user", HeaderValue::from_static(RAW));
        assert!(resolve_end_user(&cfg, &headers, br#"{"user":"body-user"}"#)
            .unwrap()
            .is_none());
    }

    #[test]
    fn schema_examples_are_not_image_inputs() {
        let cfg = server(RemoteImages::Reject, Some(1), true);
        let example = serde_json::json!({"image_url":"https://example.com/example.png"});
        for mut body in [
            serde_json::json!({"tools":[{"input_schema":{"examples":[example]}}]}),
            serde_json::json!({"functions":[{"parameters":{"examples":[example]}}]}),
            serde_json::json!({"response_format":{"json_schema":{"schema":{"examples":[example]}}}}),
            serde_json::json!({"text":{"format":{"schema":{"examples":[example]}}}}),
        ] {
            assert!(check_images(body.to_string().as_bytes(), &cfg).is_ok());
            body["messages"] = serde_json::json!([{"content":[{
                "type":"tool_result",
                "content":[{"type":"image","source":{"type":"url","url":"https://example.com/input.png"}}],
            }]}]);
            assert!(matches!(
                check_images(body.to_string().as_bytes(), &cfg),
                Err(ImagePolicyError::Remote)
            ));
        }
    }

    #[test]
    fn remote_images_and_size_limits_reject_before_any_other_use() {
        let reject = server(RemoteImages::Reject, Some(1), true);
        let remote = br#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"HTTPS://example.com/a.png"}}]}]}"#;
        let err = check_images(remote, &reject).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.message(), REMOTE_IMAGE_MESSAGE);
        let native = br#"{"messages":[{"content":[{"type":"image","source":{"type":"url","url":"http://example.com/a.png"}}]}]}"#;
        assert!(matches!(
            check_images(native, &reject),
            Err(ImagePolicyError::Remote)
        ));
        let input_image =
            br#"{"input":[{"type":"input_image","image_url":"https://example.com/a.png"}]}"#;
        assert!(matches!(
            check_images(input_image, &reject),
            Err(ImagePolicyError::Remote)
        ));

        let too_big = br#"{"messages":[{"content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,YQI="}}]}]}"#;
        let err = check_images(too_big, &reject).unwrap_err();
        assert_eq!(err.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let fits = br#"{"messages":[{"content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,YQ=="}}]}]}"#;
        assert!(check_images(fits, &reject).is_ok());
        let native_b64 = br#"{"messages":[{"content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"YQI="}}]}]}"#;
        assert!(matches!(
            check_images(native_b64, &reject),
            Err(ImagePolicyError::TooLarge)
        ));
        let invalid = br#"{"messages":[{"content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,!!!!"}}]}]}"#;
        assert!(matches!(
            check_images(invalid, &reject),
            Err(ImagePolicyError::InvalidData)
        ));

        let allow = server(RemoteImages::Allow, None, true);
        assert!(check_images(remote, &allow).is_ok());
        assert!(check_images(too_big, &allow).is_ok());
    }

    #[test]
    fn glob_match_semantics() {
        for (pattern, text) in [
            ("*", ""),
            ("*", "any.thing/at all"),
            ("claude-*", "claude-good"),
            ("claude-*", "claude-"),
            ("a/*", "a/b/c"), // `*` spans `/`
            ("a?c", "abc"),
            ("a?c", "a/c"), // `?` matches `/` too
            ("exact?y", "exactly"),
            ("abc", "abc"),
            ("a**b", "ab"), // consecutive stars collapse
            ("a*b*c", "aXYZbZc"),
            ("vendor/*", "vendor/*foo"), // literal `*` in text still wildcard-matched
            ("vendor/?", "vendor/é"),    // `?` is one character, not one byte
            ("vendor/?", "vendor/🦀"),
            ("*", "*"),
        ] {
            assert!(glob_match(pattern, text), "{pattern} ~ {text}");
        }
        for (pattern, text) in [
            ("", "x"),
            ("claude-*", "Claude-good"), // case-sensitive
            ("claude-*", "claude"),
            ("a/*", "a"),
            ("a?c", "ac"), // `?` must match exactly one character
            ("vendor/?", "vendor/"),
            ("a?c", "abbc"),
            ("abc", "abd"),
            ("a*b", "ac"),
            ("b", "abc"),
        ] {
            assert!(!glob_match(pattern, text), "{pattern} ~ {text}");
        }
    }

    #[test]
    fn key_policy_allows_except_blocked() {
        let policy = |models: &[&str], blocked: &[&str]| KeyPolicy {
            models: models.iter().map(|s| s.to_string()).collect(),
            blocked: blocked.iter().map(|s| s.to_string()).collect(),
            free_only: false,
            traffic_class: None,
        };
        let p = policy(&["claude-*", "vendor/*"], &["vendor/blocked-*"]);
        assert!(allows_model(&p, "claude-x"));
        assert!(allows_model(&p, "vendor/ok"));
        assert!(!allows_model(&p, "vendor/blocked-x"));
        assert!(!allows_model(&p, "gpt-4"));

        // Blocked-only means allow-all minus blocked.
        let p = policy(&[], &["zc/*"]);
        assert!(allows_model(&p, "anything"));
        assert!(!allows_model(&p, "zc/one"));
        // A literal `*` or multi-byte char in the name must not dodge the
        // blocked glob.
        assert!(!allows_model(&p, "zc/*sneaky"));
        assert!(!allows_model(&p, "zc/é"));
    }

    #[test]
    fn global_deny_matches_globs() {
        let deny =
            |patterns: &[&str]| -> Vec<String> { patterns.iter().map(|s| s.to_string()).collect() };
        let d = deny(&["omniroute/*", "billed-?ne"]);
        assert!(model_denied(&d, "omniroute/gpt-4"));
        assert!(model_denied(&d, "billed-one"));
        assert!(!model_denied(&d, "omniroute-x/gpt"));
        assert!(!model_denied(&d, "claude-good"));
        assert!(!model_denied(&deny(&[]), "anything"));
    }
}
