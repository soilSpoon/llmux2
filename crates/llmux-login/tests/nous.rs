use axum::{extract::State, routing::post, Router};
use llmux_login::nous::{snippet, NousLogin};
use llmux_testkit::TestServer;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use url::Url;

#[derive(Default)]
struct Portal {
    polls: AtomicUsize,
    device_form: Mutex<String>,
    poll_forms: Mutex<Vec<String>>,
    deny: bool,
}

async fn device(State(p): State<Arc<Portal>>, body: String) -> String {
    *p.device_form.lock().unwrap() = body;
    r#"{"device_code":"dev-code","user_code":"ABCD-1234","verification_uri":"https://portal.test/device","verification_uri_complete":"https://portal.test/device?user_code=ABCD-1234","expires_in":60,"interval":1}"#.into()
}

async fn token(State(p): State<Arc<Portal>>, body: String) -> (http::StatusCode, String) {
    p.poll_forms.lock().unwrap().push(body);
    let n = p.polls.fetch_add(1, Ordering::Relaxed);
    if p.deny {
        return (
            http::StatusCode::BAD_REQUEST,
            r#"{"error":"access_denied"}"#.into(),
        );
    }
    match n {
        0 => (http::StatusCode::BAD_REQUEST, r#"{"error":"authorization_pending"}"#.into()),
        _ => (
            http::StatusCode::OK,
            r#"{"access_token":"synthetic-access","refresh_token":"synthetic-refresh","token_type":"Bearer","scope":"inference:invoke","expires_in":900}"#.into(),
        ),
    }
}

async fn server(portal: Arc<Portal>) -> TestServer {
    TestServer::start(
        Router::new()
            .route("/api/oauth/device/code", post(device))
            .route("/api/oauth/token", post(token))
            .with_state(portal),
    )
    .await
    .unwrap()
}

fn login(server: &TestServer) -> NousLogin {
    NousLogin {
        portal: Url::parse(&server.url()).unwrap(),
        min_poll: Duration::from_millis(10),
        ..Default::default()
    }
}

#[tokio::test]
async fn approval_writes_private_state_and_announces_url_and_code() {
    let portal = Arc::new(Portal::default());
    let server = server(portal.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nous").join("state.json");
    let mut announced = None;
    let state = login(&server)
        .run(&path, |url, code| {
            announced = Some((url.to_owned(), code.to_owned()))
        })
        .await
        .unwrap();
    assert_eq!(
        announced.unwrap(),
        (
            "https://portal.test/device?user_code=ABCD-1234".into(),
            "ABCD-1234".into()
        )
    );
    assert_eq!(
        *portal.device_form.lock().unwrap(),
        "client_id=hermes-cli&scope=inference%3Ainvoke"
    );
    let forms = portal.poll_forms.lock().unwrap();
    assert_eq!(forms.len(), 2);
    assert_eq!(
        forms[0],
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&client_id=hermes-cli&device_code=dev-code"
    );
    assert_eq!(state.refresh_token, "synthetic-refresh");
    let saved = llmux_types::NousState::load(&path).unwrap();
    assert_eq!(saved.access_token, "synthetic-access");
    assert_eq!(saved.portal_base_url, server.url());
    assert!(saved.expires_at > 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }
    let snippet = snippet(&path);
    assert!(snippet.contains("kind = \"nous_portal\""));
    assert!(snippet.contains("state_file ="));
    assert!(!snippet.contains("synthetic"));
}

#[tokio::test]
async fn denied_approval_writes_nothing() {
    let portal = Arc::new(Portal {
        deny: true,
        ..Default::default()
    });
    let server = server(portal).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let error = login(&server).run(&path, |_, _| {}).await.err().unwrap();
    assert!(matches!(error, llmux_login::Error::AccessDenied));
    assert!(!path.exists());
}
