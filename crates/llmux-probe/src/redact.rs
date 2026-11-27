//! Redaction and hashing for the evidence record.
//!
//! The prompt text and every credential-shaped value are replaced before any
//! bytes reach the record or a log line. Hashes are kept so a reviewer can tie
//! the record to the exact request without storing it.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const EXCERPT_MAX_CHARS: usize = 2_048;
const MESSAGE_PLACEHOLDER: &str = "<redacted:message-content>";
const CREDENTIAL_PLACEHOLDER: &str = "<redacted:credential>";

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Stable JSON re-serialization of an outbound body with message content and
/// credential fields replaced by fixed placeholders.
pub fn canonical_redacted_body(bytes: &[u8]) -> Result<(Value, String), String> {
    let mut value: Value =
        serde_json::from_slice(bytes).map_err(|err| format!("outbound body is not JSON: {err}"))?;
    redact_body(&mut value);
    let canonical =
        serde_json::to_vec(&value).map_err(|err| format!("redacted body is not JSON: {err}"))?;
    Ok((value, sha256_hex(&canonical)))
}

fn redact_body(value: &mut Value) {
    if let Some(messages) = value.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if let Some(object) = message.as_object_mut() {
                if object.contains_key("content") {
                    object.insert("content".into(), json!(MESSAGE_PLACEHOLDER));
                }
            }
        }
    }
    if let Some(object) = value.as_object_mut() {
        for key in ["authorization", "api_key", "api-key", "token", "secret"] {
            if object.contains_key(key) {
                object.insert(key.into(), json!(CREDENTIAL_PLACEHOLDER));
            }
        }
    }
}

/// Replace every supplied secret and every credential-shaped token, then bound
/// the excerpt so a provider cannot grow the record.
pub fn redact_text(raw: &str, secrets: &[(&str, &str)]) -> String {
    let mut text = raw.to_owned();
    for (secret, placeholder) in secrets {
        if !secret.is_empty() {
            text = text.replace(secret, placeholder);
        }
    }
    let text = scrub_tokens(&text);
    text.chars().take(EXCERPT_MAX_CHARS).collect()
}

fn scrub_tokens(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if starts_with_ignore_ascii_case(&bytes[index..], b"bearer ") {
            out.push_str("Bearer <redacted:authorization>");
            index += b"bearer ".len();
            while index < bytes.len() && is_token_byte(bytes[index]) {
                index += 1;
            }
            continue;
        }
        if bytes[index..].starts_with(b"ses_") || bytes[index..].starts_with(b"prj_") {
            let is_session = bytes[index..].starts_with(b"ses_");
            out.push_str(if is_session {
                "<redacted:session>"
            } else {
                "<redacted:project>"
            });
            index += 4;
            let mut skipped = 0;
            while index < bytes.len() && is_token_byte(bytes[index]) && skipped < 64 {
                index += 1;
                skipped += 1;
            }
            continue;
        }
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

fn starts_with_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'+' | b'/')
        || byte == b'='
}
