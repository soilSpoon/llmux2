use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use bytes::Bytes;
use http::{Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use llmux_login::{Endpoints, Flow, PendingLogin};
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use url::Url;

// Public, disposable RSA fixtures used only to sign fake issuer responses.
const KEY: &[u8] = include_bytes!("fixtures/rsa.pem");
const WRONG_KEY: &[u8] = include_bytes!("fixtures/wrong-rsa.pem");
const MODULUS: &str = "lIUhiRxKLC2-f2qvAx3XHqKOcWkfMc_gpurKIihs_f3L8mU4fveMmfoHgjKxqVLV7DDCApS73ncc35nz72Q7LcwraEY4CQhge0X7U6hZKhWDuZjumDIkWmlS49cUAsDGmGsgsWVKkDCtpEoTBa9c5rMHYD8TzzuMLi2LQdJ6seehDF_a346avgruQHk6WmID_T54zI2bUcFmMgiwsKLMYXKDl2z3C3el5INMl00A0P_sjcqxNb4Sy3DNPVQJ0F7nxfvpz9g6SiQgmtcNe6Vxsr6OIYolFbaRyJergKmyAACY7aVB_gvYERQhy0GDpQIEW66KMxlIXqLMDlmUYBvAjw";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

#[derive(Default)]
struct Fake {
    issuer: String,
    nonce: String,
    challenge: String,
    redirect: String,
    mode: &'static str,
    exchanges: usize,
}

async fn token(State(state): State<Arc<Mutex<Fake>>>, body: String) -> (StatusCode, Json<Value>) {
    let mut fake = state.lock().await;
    fake.exchanges += 1;
    let fields: HashMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(fields.len(), 6);
    assert_eq!(fields["grant_type"], "authorization_code");
    assert_eq!(fields["client_id"], "oaiapp_issued");
    assert_eq!(fields["code"], "code +&");
    assert_eq!(fields["redirect_uri"], fake.redirect);
    assert_eq!(fields["resource"], "https://api.openai.com/v1");
    let verifier = &fields["code_verifier"];
    assert!((43..=128).contains(&verifier.len()));
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier)),
        fake.challenge
    );
    if fake.mode == "invalid_grant" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_grant","error_description":"never log code or tokens"})),
        );
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut claims = json!({"iss":fake.issuer,"aud":"oaiapp_issued","sub":"subject","email":"user@example.com","nonce":fake.nonce,"exp":now+3600});
    match fake.mode {
        "aud" => claims["aud"] = json!("wrong-client"),
        "nonce" => claims["nonce"] = json!("wrong-nonce"),
        "no_nonce" => {
            claims.as_object_mut().unwrap().remove("nonce");
        }
        "expired" => claims["exp"] = json!(now - 3600),
        "issuer" => claims["iss"] = json!("https://wrong.example"),
        "subject" => claims["sub"] = json!("other-account"),
        _ => {}
    }
    let key = if fake.mode == "signature" {
        WRONG_KEY
    } else {
        KEY
    };
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test-key".into());
    let id = encode(&header, &claims, &EncodingKey::from_rsa_pem(key).unwrap()).unwrap();
    let mut response = json!({"access_token":"access","refresh_token":"refresh","id_token":id,"token_type":"Bearer","expires_in":3600,"scope":if fake.mode == "scope" { "openid email" } else { SCOPES }});
    if fake.mode == "absent_scope" {
        response.as_object_mut().unwrap().remove("scope");
    }
    (StatusCode::OK, Json(response))
}

async fn fixture() -> (TestServer, Arc<Mutex<Fake>>, Endpoints) {
    let state = Arc::new(Mutex::new(Fake::default()));
    let router = Router::new()
        .route("/token", post(token))
        .route("/.well-known/openid-configuration", get(|State(state): State<Arc<Mutex<Fake>>>| async move {
            let fake = state.lock().await;
            Json(json!({"issuer":fake.issuer,"jwks_uri":format!("{}/jwks",fake.issuer)}))
        }))
        .route("/jwks", get(|| async { Json(json!({"keys":[{"kty":"RSA","kid":"test-key","use":"sig","alg":"RS256","n":MODULUS,"e":"AQAB"}]})) }))
        .with_state(state.clone());
    let server = TestServer::start(router).await.unwrap();
    state.lock().await.issuer = server.url();
    let endpoints = Endpoints {
        authorize: Url::parse(&format!("{}/authorize", server.url())).unwrap(),
        token: Url::parse(&format!("{}/token", server.url())).unwrap(),
        issuer: Url::parse(&server.url()).unwrap(),
    };
    (server, state, endpoints)
}

fn params(pending: &PendingLogin) -> HashMap<String, String> {
    pending
        .authorization_url()
        .query_pairs()
        .into_owned()
        .collect()
}

async fn prepare(fake: &Arc<Mutex<Fake>>, params: &HashMap<String, String>) {
    let mut fake = fake.lock().await;
    fake.nonce = params["nonce"].clone();
    fake.challenge = params["code_challenge"].clone();
    fake.redirect = params["redirect_uri"].clone();
}

async fn callback(params: &HashMap<String, String>, values: &[(&str, &str)]) -> StatusCode {
    let mut url = Url::parse(&params["redirect_uri"]).unwrap();
    url.query_pairs_mut().extend_pairs(values.iter().copied());
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let response = client
        .request(
            Request::get(url.as_str())
                .body(Full::new(Bytes::new()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    response.into_body().collect().await.unwrap();
    status
}

#[tokio::test]
async fn first_login_files_snippet_and_returning_login() {
    let (_server, fake, endpoints) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let pending = PendingLogin::start(dir.path(), Flow::codex(endpoints.clone()))
        .await
        .unwrap();
    let first = params(&pending);
    assert_eq!(first["client_id"], "dynamic_agent_client");
    assert_eq!(first["agent_name_hint"], "llmux");
    assert_eq!(first["response_type"], "code");
    assert_eq!(first["scope"], SCOPES);
    assert_eq!(first["resource"], "https://api.openai.com/v1");
    assert_eq!(first["code_challenge_method"], "S256");
    let host = first["ext_agent_host_id"]
        .strip_prefix("urn:uuid:")
        .unwrap();
    assert_eq!(uuid::Uuid::parse_str(host).unwrap().get_version_num(), 4);
    assert!(!first.contains_key("id_token_hint"));
    let redirect = Url::parse(&first["redirect_uri"]).unwrap();
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    assert_eq!(redirect.path(), "/auth/callback");
    prepare(&fake, &first).await;
    let complete = tokio::spawn(pending.finish());
    assert_eq!(
        callback(
            &first,
            &[
                ("state", &first["state"]),
                ("code", "code +&"),
                ("client_id", "oaiapp_issued")
            ]
        )
        .await,
        StatusCode::OK
    );
    let result = complete.await.unwrap().unwrap();
    let record: Value =
        serde_json::from_slice(&fs::read(dir.path().join("credentials.json")).unwrap()).unwrap();
    assert_eq!(record["client_id"], "oaiapp_issued");
    assert_eq!(record["subject"], "subject");
    assert_eq!(record["email"], "user@example.com");
    assert_eq!(record["issuer"], fake.lock().await.issuer);
    assert_eq!(record["ext_agent_host_id"], first["ext_agent_host_id"]);
    assert_eq!(record["access_token"], "access");
    assert_eq!(record["refresh_token"], "refresh");
    assert_eq!(record["expires_in"], 3600);
    assert_eq!(record["token_type"], "Bearer");
    assert_eq!(
        record["scopes"],
        json!(SCOPES.split_whitespace().collect::<Vec<_>>())
    );
    assert!(chrono::DateTime::parse_from_rfc3339(record["saved_at"].as_str().unwrap()).is_ok());
    assert_eq!(
        fs::read_to_string(dir.path().join("refresh_token")).unwrap(),
        "refresh"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["credentials.json", "refresh_token", "host_id"] {
            assert_eq!(
                fs::metadata(dir.path().join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    let snippet: toml::Value = toml::from_str(&result.snippet).unwrap();
    let credential = &snippet["credentials"][0];
    assert_eq!(credential["kind"].as_str(), Some("oauth"));
    assert_eq!(credential["provider"].as_str(), Some("openai_responses"));
    assert_eq!(credential["client_id"].as_str(), Some("oaiapp_issued"));
    assert_eq!(
        credential["token_url"].as_str(),
        Some(endpoints.token.as_str())
    );
    assert_eq!(
        credential["resource"].as_str(),
        Some("https://api.openai.com/v1")
    );
    assert_eq!(
        credential["refresh_token"].as_str().unwrap(),
        format!("file:{}", dir.path().join("refresh_token").display())
    );
    let pending = PendingLogin::start(dir.path(), Flow::codex(endpoints))
        .await
        .unwrap();
    let second = params(&pending);
    assert_eq!(second["ext_agent_host_id"], first["ext_agent_host_id"]);
    assert_eq!(second["client_id"], "oaiapp_issued");
    assert_eq!(
        second["id_token_hint"],
        record["id_token"].as_str().unwrap()
    );
    assert_eq!(second["login_hint"], "user@example.com");
    assert!(!second.contains_key("agent_name_hint"));
    for key in ["state", "nonce", "code_challenge"] {
        assert_ne!(second[key], first[key]);
    }
    prepare(&fake, &second).await;
    let complete = tokio::spawn(pending.finish());
    callback(&second, &[("state", &second["state"]), ("code", "code +&")]).await;
    complete.await.unwrap().unwrap();
}

#[tokio::test]
async fn invalid_callback_state_keeps_waiting_for_valid_callback() {
    let (_server, fake, endpoints) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let pending = PendingLogin::start(dir.path(), Flow::codex(endpoints))
        .await
        .unwrap();
    let params = params(&pending);
    prepare(&fake, &params).await;
    let complete = tokio::spawn(pending.finish());
    for fields in [
        vec![("error", "access_denied")],
        vec![("state", "wrong"), ("error", "access_denied")],
        vec![("state", &params["state"]), ("state", "wrong")],
    ] {
        assert_eq!(callback(&params, &fields).await, StatusCode::BAD_REQUEST);
        assert!(!complete.is_finished(), "invalid state ended pending login");
        assert_eq!(fake.lock().await.exchanges, 0);
        assert!(!dir.path().join("credentials.json").exists());
        assert!(!dir.path().join("refresh_token").exists());
    }
    assert_eq!(
        callback(
            &params,
            &[
                ("state", &params["state"]),
                ("code", "code +&"),
                ("client_id", "oaiapp_issued"),
            ],
        )
        .await,
        StatusCode::OK
    );
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), complete)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.credentials.client_id, "oaiapp_issued");
    assert_eq!(fake.lock().await.exchanges, 1);
    assert!(dir.path().join("credentials.json").exists());
}

#[tokio::test]
async fn callback_failures_do_not_exchange_or_write_credentials() {
    for mode in ["denied", "missing", "dynamic", "duplicate"] {
        let (_server, fake, endpoints) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let pending = PendingLogin::start(dir.path(), Flow::codex(endpoints))
            .await
            .unwrap();
        let params = params(&pending);
        let state = params["state"].as_str();
        let mut fields = vec![("state", state), ("code", "code +&")];
        match mode {
            "denied" => fields.push(("error", "access_denied")),
            "dynamic" => fields.push(("client_id", "dynamic_agent_client")),
            "duplicate" => {
                fields.push(("code", "duplicate"));
                fields.push(("client_id", "oaiapp_issued"));
            }
            _ => {}
        }
        let complete = tokio::spawn(pending.finish());
        assert_eq!(callback(&params, &fields).await, StatusCode::BAD_REQUEST);
        let error = complete.await.unwrap().err().unwrap().to_string();
        if mode == "denied" {
            assert!(error.contains("denied"));
        }
        if mode == "missing" {
            assert!(error.contains("incomplete"));
        }
        assert_eq!(fake.lock().await.exchanges, 0);
        assert!(!dir.path().join("credentials.json").exists());
        assert!(!dir.path().join("refresh_token").exists());
    }
}

#[tokio::test]
async fn invalid_tokens_and_missing_plan_permission_are_rejected() {
    for mode in [
        "signature",
        "aud",
        "nonce",
        "no_nonce",
        "expired",
        "issuer",
        "scope",
        "absent_scope",
    ] {
        let (_server, fake, endpoints) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        fake.lock().await.mode = mode;
        let pending = PendingLogin::start(dir.path(), Flow::codex(endpoints))
            .await
            .unwrap();
        let params = params(&pending);
        prepare(&fake, &params).await;
        let complete = tokio::spawn(pending.finish());
        callback(
            &params,
            &[
                ("state", &params["state"]),
                ("code", "code +&"),
                ("client_id", "oaiapp_issued"),
            ],
        )
        .await;
        let error = complete.await.unwrap().err().unwrap().to_string();
        if matches!(mode, "scope" | "absent_scope") {
            assert!(error.contains("chatgpt.tokens.use.direct"));
        }
        assert!(!dir.path().join("credentials.json").exists());
        assert!(!dir.path().join("refresh_token").exists());
    }
}

#[tokio::test]
async fn returning_login_rejects_client_or_identity_change_without_overwriting() {
    let (_server, fake, endpoints) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let pending = PendingLogin::start(dir.path(), Flow::codex(endpoints.clone()))
        .await
        .unwrap();
    let p = params(&pending);
    prepare(&fake, &p).await;
    let task = tokio::spawn(pending.finish());
    callback(
        &p,
        &[
            ("state", &p["state"]),
            ("code", "code +&"),
            ("client_id", "oaiapp_issued"),
        ],
    )
    .await;
    task.await.unwrap().unwrap();
    let before = fs::read(dir.path().join("credentials.json")).unwrap();
    for mode in ["client", "subject"] {
        let pending = PendingLogin::start(dir.path(), Flow::codex(endpoints.clone()))
            .await
            .unwrap();
        let p = params(&pending);
        prepare(&fake, &p).await;
        fake.lock().await.mode = mode;
        let task = tokio::spawn(pending.finish());
        let client = if mode == "client" {
            "other-client"
        } else {
            "oaiapp_issued"
        };
        callback(
            &p,
            &[
                ("state", &p["state"]),
                ("code", "code +&"),
                ("client_id", client),
            ],
        )
        .await;
        assert!(task.await.unwrap().is_err());
        assert_eq!(
            fs::read(dir.path().join("credentials.json")).unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn invalid_grant_restarts_with_the_issued_client_and_fresh_pkce() {
    let (_server, fake, endpoints) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    fake.lock().await.mode = "invalid_grant";
    let pending = PendingLogin::start(dir.path(), Flow::codex(endpoints.clone()))
        .await
        .unwrap();
    let first = params(&pending);
    prepare(&fake, &first).await;
    let task = tokio::spawn(pending.finish());
    callback(
        &first,
        &[
            ("state", &first["state"]),
            ("code", "code +&"),
            ("client_id", "oaiapp_issued"),
        ],
    )
    .await;
    let error = task.await.unwrap().err().unwrap();
    let client = error.retry_client_id().unwrap();
    assert_eq!(client, "oaiapp_issued");
    assert!(!dir.path().join("credentials.json").exists());
    let pending = PendingLogin::retry(dir.path(), Flow::codex(endpoints), client)
        .await
        .unwrap();
    let next = params(&pending);
    assert_eq!(next["client_id"], client);
    assert_eq!(next["ext_agent_host_id"], first["ext_agent_host_id"]);
    assert!(!next.contains_key("agent_name_hint"));
    for key in ["state", "nonce", "code_challenge"] {
        assert_ne!(next[key], first[key]);
    }
    fake.lock().await.mode = "";
    prepare(&fake, &next).await;
    let task = tokio::spawn(pending.finish());
    callback(&next, &[("state", &next["state"]), ("code", "code +&")]).await;
    task.await.unwrap().unwrap();
}

#[test]
fn cli_help_and_invalid_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_llmux-login");
    let help = std::process::Command::new(binary)
        .arg("--help")
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8(help.stdout).unwrap().contains("--dir"));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    for args in [
        vec!["--dir"],
        vec!["--issuer", "http://example.com"],
        vec!["--dir", "/unused", "--dir", "/unused"],
    ] {
        let result = std::process::Command::new(binary)
            .args(args)
            .output()
            .unwrap();
        assert!(!result.status.success());
    }
}

#[tokio::test]
async fn occupied_preferred_port_uses_another_loopback_port() {
    let occupied = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:1455").await {
                break listener;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (_server, _fake, endpoints) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let pending = PendingLogin::start(dir.path(), Flow::codex(endpoints))
        .await
        .unwrap();
    let p = params(&pending);
    let redirect = Url::parse(&p["redirect_uri"]).unwrap();
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    assert_ne!(redirect.port(), Some(1455));
    drop(occupied);
}
