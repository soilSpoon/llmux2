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
    agents: Mutex<Vec<String>>,
    script: Mutex<Vec<(http::StatusCode, &'static str, &'static str)>>,
    expires_in: u64,
    repeat_last: bool,
    deny: bool,
}

async fn device(State(p): State<Arc<Portal>>, headers: http::HeaderMap, body: String) -> String {
    p.agents.lock().unwrap().push(
        headers
            .get("user-agent")
            .map_or(String::new(), |v| v.to_str().unwrap().into()),
    );
    *p.device_form.lock().unwrap() = body;
    format!(
        r#"{{"device_code":"dev-code","user_code":"ABCD-1234","verification_uri":"https://portal.test/device","verification_uri_complete":"https://portal.test/device?user_code=ABCD-1234","expires_in":{},"interval":0}}"#,
        if p.expires_in == 0 { 60 } else { p.expires_in }
    )
}

async fn token(
    State(p): State<Arc<Portal>>,
    headers: http::HeaderMap,
    body: String,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    p.agents.lock().unwrap().push(
        headers
            .get("user-agent")
            .map_or(String::new(), |v| v.to_str().unwrap().into()),
    );
    p.poll_forms.lock().unwrap().push(body);
    let n = p.polls.fetch_add(1, Ordering::Relaxed);
    let scripted = {
        let script = p.script.lock().unwrap();
        script
            .get(n)
            .or_else(|| script.last().filter(|_| p.repeat_last))
            .copied()
    };
    if let Some((status, content_type, body)) = scripted {
        return (status, [("content-type", content_type)], body).into_response();
    }
    let skipped = p.script.lock().unwrap().len();
    token_default(&p, n.saturating_sub(skipped)).into_response()
}

fn token_default(p: &Portal, n: usize) -> (http::StatusCode, String) {
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

fn scripted(
    script: Vec<(http::StatusCode, &'static str, &'static str)>,
    repeat_last: bool,
    expires_in: u64,
) -> Arc<Portal> {
    Arc::new(Portal {
        script: Mutex::new(script),
        repeat_last,
        expires_in,
        ..Default::default()
    })
}

#[tokio::test]
async fn every_request_carries_a_user_agent() {
    let portal = Arc::new(Portal::default());
    let server = server(portal.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    login(&server)
        .run(&dir.path().join("state.json"), |_, _| {})
        .await
        .unwrap();
    let agents = portal.agents.lock().unwrap();
    assert_eq!(agents.len(), 3);
    for agent in agents.iter() {
        assert_eq!(*agent, format!("llmux/{}", env!("CARGO_PKG_VERSION")));
    }
}

#[tokio::test]
async fn transient_non_oauth_poll_responses_keep_polling_until_approval() {
    use http::StatusCode as S;
    let portal = scripted(
        vec![
            (S::FORBIDDEN, "text/plain", "Forbidden"),
            (S::TOO_MANY_REQUESTS, "text/plain", "Too Many Requests"),
            (S::BAD_GATEWAY, "text/html", "<html>502</html>"),
            (S::SERVICE_UNAVAILABLE, "application/json", "{}"),
            (S::OK, "text/html", "<html>challenge</html>"),
            (
                S::BAD_REQUEST,
                "application/json",
                r#"{"error":"slow_down"}"#,
            ),
            (
                S::BAD_REQUEST,
                "application/json",
                r#"{"error":"authorization_pending"}"#,
            ),
        ],
        false,
        60,
    );
    let server = server(portal.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let state = login(&server).run(&path, |_, _| {}).await.unwrap();
    assert_eq!(state.access_token, "synthetic-access");
    assert_eq!(portal.polls.load(Ordering::Relaxed), 9);
}

#[tokio::test]
async fn persistent_transient_failure_times_out_and_reports_what_was_received() {
    use http::StatusCode as S;
    let portal = scripted(
        vec![(S::FORBIDDEN, "text/plain", "Forbidden by edge")],
        true,
        1,
    );
    let server = server(portal).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let error = login(&server).run(&path, |_, _| {}).await.err().unwrap();
    let message = error.to_string();
    assert!(matches!(error, llmux_login::Error::Poll(_)), "{message}");
    assert!(message.contains("403"), "{message}");
    assert!(message.contains("Forbidden by edge"), "{message}");
    assert!(!message.contains("dev-code"), "{message}");
    assert!(!path.exists());
}

#[tokio::test]
async fn definitive_oauth_error_fails_immediately_with_the_portal_code() {
    use http::StatusCode as S;
    let portal = scripted(
        vec![(
            S::BAD_REQUEST,
            "application/json",
            r#"{"error":"invalid_client","error_description":"unknown client"}"#,
        )],
        false,
        60,
    );
    let server = server(portal.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let error = login(&server)
        .run(&dir.path().join("state.json"), |_, _| {})
        .await
        .err()
        .unwrap();
    let message = error.to_string();
    assert!(message.contains("invalid_client"), "{message}");
    assert_eq!(portal.polls.load(Ordering::Relaxed), 1);
}
