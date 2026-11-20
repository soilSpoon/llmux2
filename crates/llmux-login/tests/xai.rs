//! xAI OAuth (api.x.ai) authorization-code PKCE login.

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

// Public, disposable P-256 fixtures used only to sign fake issuer responses.
const KEY: &[u8] = include_bytes!("fixtures/ec.pem");
const WRONG_KEY: &[u8] = include_bytes!("fixtures/wrong-ec.pem");
const X: &str = "uHfyTA2V8j56b8_GsxXNaEj3l9rvisjxMbHvijrBM4U";
const Y: &str = "JEeFwrBbbCeu_KBz8_LbU_-CN7_LAeLJNvKpeJAu2GI";
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPES: &str = "openid profile email offline_access grok-cli:access api:access";

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
    assert_eq!(fields.len(), 5, "xAI exchange sends no extra fields");
    assert_eq!(fields["grant_type"], "authorization_code");
    assert_eq!(fields["client_id"], CLIENT_ID);
    assert_eq!(fields["code"], "code +&");
    assert_eq!(fields["redirect_uri"], fake.redirect);
    let verifier = &fields["code_verifier"];
    assert_eq!(verifier.len(), 128, "xAI uses a 96-byte PKCE verifier");
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
    let mut claims = json!({"iss":fake.issuer,"aud":CLIENT_ID,"sub":"xai-subject","email":"xai@example.com","nonce":fake.nonce,"exp":now+3600});
    match fake.mode {
        "aud" => claims["aud"] = json!("wrong-client"),
        "nonce" => claims["nonce"] = json!("wrong-nonce"),
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
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some("test-key".into());
    let id = encode(&header, &claims, &EncodingKey::from_ec_pem(key).unwrap()).unwrap();
    let mut response = json!({"access_token":"access","refresh_token":"refresh","id_token":id,"token_type":"Bearer","expires_in":21600,"scope":if fake.mode == "scope" { "openid email" } else { SCOPES }});
    if fake.mode == "absent_scope" {
        response.as_object_mut().unwrap().remove("scope");
    }
    (StatusCode::OK, Json(response))
}

async fn fixture() -> (TestServer, Arc<Mutex<Fake>>, Endpoints) {
    let state = Arc::new(Mutex::new(Fake::default()));
    let router = Router::new()
        .route("/token", post(token))
        .route(
            "/.well-known/openid-configuration",
            get(|State(state): State<Arc<Mutex<Fake>>>| async move {
                let fake = state.lock().await;
                Json(json!({"issuer":fake.issuer,"jwks_uri":format!("{}/jwks",fake.issuer)}))
            }),
        )
        .route(
            "/jwks",
            get(|| async {
                Json(json!({"keys":[{"kty":"EC","crv":"P-256","kid":"test-key","use":"sig","alg":"ES256","x":X,"y":Y}]}))
            }),
        )
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
async fn xai_login_files_snippet_and_returning_login() {
    let (_server, fake, endpoints) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let pending = PendingLogin::start(dir.path(), Flow::xai(endpoints.clone()))
        .await
        .unwrap();
    let first = params(&pending);
    assert_eq!(first["client_id"], CLIENT_ID);
    assert_eq!(first["response_type"], "code");
    assert_eq!(first["scope"], SCOPES);
    assert_eq!(first["code_challenge_method"], "S256");
    assert_eq!(first["plan"], "generic");
    assert_eq!(first["referrer"], "cli-proxy-api");
    assert!(!first.contains_key("resource"));
    assert!(!first.contains_key("ext_agent_host_id"));
    assert!(!first.contains_key("agent_name_hint"));
    assert!(!first.contains_key("id_token_hint"));
    let redirect = Url::parse(&first["redirect_uri"]).unwrap();
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    assert_eq!(redirect.path(), "/callback");
    prepare(&fake, &first).await;
    let complete = tokio::spawn(pending.finish());
    assert_eq!(
        callback(&first, &[("state", &first["state"]), ("code", "code +&")]).await,
        StatusCode::OK
    );
    let result = complete.await.unwrap().unwrap();
    let record: Value =
        serde_json::from_slice(&fs::read(dir.path().join("credentials.json")).unwrap()).unwrap();
    assert_eq!(record["client_id"], CLIENT_ID);
    assert_eq!(record["subject"], "xai-subject");
    assert_eq!(record["email"], "xai@example.com");
    assert_eq!(record["issuer"], fake.lock().await.issuer);
    assert_eq!(record["access_token"], "access");
    assert_eq!(record["refresh_token"], "refresh");
    assert_eq!(record["expires_in"], 21600);
    assert_eq!(
        record["scopes"],
        json!(SCOPES.split_whitespace().collect::<Vec<_>>())
    );
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
    assert_eq!(credential["id"].as_str(), Some("xai"));
    assert_eq!(credential["kind"].as_str(), Some("oauth"));
    assert_eq!(credential["provider"].as_str(), Some("openai_compat"));
    assert_eq!(credential["client_id"].as_str(), Some(CLIENT_ID));
    assert_eq!(
        credential["token_url"].as_str(),
        Some(endpoints.token.as_str())
    );
    assert!(credential.get("resource").is_none());
    assert_eq!(
        credential["refresh_token"].as_str().unwrap(),
        format!("file:{}", dir.path().join("refresh_token").display())
    );

    let pending = PendingLogin::start(dir.path(), Flow::xai(endpoints))
        .await
        .unwrap();
    let second = params(&pending);
    assert_eq!(second["client_id"], CLIENT_ID);
    assert_eq!(
        second["id_token_hint"],
        record["id_token"].as_str().unwrap()
    );
    assert_eq!(second["login_hint"], "xai@example.com");
    for key in ["state", "nonce", "code_challenge"] {
        assert_ne!(second[key], first[key]);
    }
    prepare(&fake, &second).await;
    let complete = tokio::spawn(pending.finish());
    callback(&second, &[("state", &second["state"]), ("code", "code +&")]).await;
    complete.await.unwrap().unwrap();
}

#[tokio::test]
async fn xai_callback_failures_do_not_exchange_or_write_credentials() {
    // A fixed public client needs no client_id on the callback, unlike codex's
    // dynamic registration; "missing" is a success case, not a failure mode.
    for mode in ["denied", "duplicate"] {
        let (_server, fake, endpoints) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let pending = PendingLogin::start(dir.path(), Flow::xai(endpoints))
            .await
            .unwrap();
        let params = params(&pending);
        let state = params["state"].as_str();
        let mut fields = vec![("state", state), ("code", "code +&")];
        match mode {
            "denied" => fields.push(("error", "access_denied")),
            "duplicate" => {
                fields.push(("code", "duplicate"));
                fields.push(("client_id", CLIENT_ID));
            }
            _ => {}
        }
        let complete = tokio::spawn(pending.finish());
        assert_eq!(callback(&params, &fields).await, StatusCode::BAD_REQUEST);
        complete.await.unwrap().err().unwrap();
        assert_eq!(fake.lock().await.exchanges, 0);
        assert!(!dir.path().join("credentials.json").exists());
        assert!(!dir.path().join("refresh_token").exists());
    }
}

#[tokio::test]
async fn xai_invalid_tokens_and_missing_api_scope_are_rejected() {
    for mode in [
        "signature",
        "aud",
        "nonce",
        "expired",
        "issuer",
        "scope",
        "absent_scope",
    ] {
        let (_server, fake, endpoints) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        fake.lock().await.mode = mode;
        let pending = PendingLogin::start(dir.path(), Flow::xai(endpoints))
            .await
            .unwrap();
        let params = params(&pending);
        prepare(&fake, &params).await;
        let complete = tokio::spawn(pending.finish());
        callback(&params, &[("state", &params["state"]), ("code", "code +&")]).await;
        let error = complete.await.unwrap().err().unwrap().to_string();
        if matches!(mode, "scope" | "absent_scope") {
            assert!(error.contains("api:access"), "{error}");
        }
        assert!(!dir.path().join("credentials.json").exists());
        assert!(!dir.path().join("refresh_token").exists());
    }
}

#[tokio::test]
async fn xai_returning_login_rejects_identity_change_without_overwriting() {
    let (_server, fake, endpoints) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let pending = PendingLogin::start(dir.path(), Flow::xai(endpoints.clone()))
        .await
        .unwrap();
    let p = params(&pending);
    prepare(&fake, &p).await;
    let task = tokio::spawn(pending.finish());
    callback(&p, &[("state", &p["state"]), ("code", "code +&")]).await;
    task.await.unwrap().unwrap();
    let before = fs::read(dir.path().join("credentials.json")).unwrap();
    fake.lock().await.mode = "subject";
    let pending = PendingLogin::start(dir.path(), Flow::xai(endpoints))
        .await
        .unwrap();
    let p = params(&pending);
    prepare(&fake, &p).await;
    let task = tokio::spawn(pending.finish());
    callback(&p, &[("state", &p["state"]), ("code", "code +&")]).await;
    assert!(task.await.unwrap().is_err());
    assert_eq!(
        fs::read(dir.path().join("credentials.json")).unwrap(),
        before
    );
}
