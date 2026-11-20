use axum::{body::Body, routing::get, Router};
use http::Request;
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{oneshot, Semaphore},
    time::timeout,
};

#[tokio::test]
async fn listeners_share_port() {
    let first = llmux_server::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let second = llmux_server::bind(first.local_addr().unwrap()).await;
    assert!(second.is_ok());
}

async fn drain_test(streaming: bool, force: bool) {
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (started, gate) = (entered.clone(), release.clone());
    let router = Router::new().route(
        "/slow",
        get(move || {
            let (started, gate) = (started.clone(), gate.clone());
            async move {
                if streaming {
                    Body::from_stream(futures_util::stream::once(async move {
                        started.add_permits(1);
                        gate.acquire().await.unwrap().forget();
                        Ok::<_, std::io::Error>("data: done\n\n")
                    }))
                } else {
                    started.add_permits(1);
                    gate.acquire().await.unwrap().forget();
                    Body::from("done")
                }
            }
        }),
    );
    let listener = llmux_server::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, shutdown) = oneshot::channel();
    let task = tokio::spawn(llmux_server::serve(
        listener,
        router,
        async {
            let _ = shutdown.await;
        },
        Duration::from_millis(if force { 30 } else { 1000 }),
        Duration::ZERO,
    ));
    let request = tokio::spawn(async move {
        let response = Client::builder(TokioExecutor::new())
            .build_http()
            .request(
                Request::get(format!("http://{address}/slow"))
                    .body(Full::<axum::body::Bytes>::default())
                    .unwrap(),
            )
            .await
            .unwrap();
        response.into_body().collect().await.unwrap().to_bytes()
    });
    timeout(Duration::from_secs(2), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    stop.send(()).unwrap();
    if !force {
        timeout(Duration::from_secs(2), async {
            while tokio::net::TcpStream::connect(address).await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !task.is_finished(),
            "in-flight work must keep serving alive"
        );
        release.add_permits(1);
        let result = request.await.unwrap();
        assert_eq!(
            &result[..],
            if streaming {
                b"data: done\n\n" as &[u8]
            } else {
                b"done"
            }
        );
    } else {
        timeout(Duration::from_secs(2), task)
            .await
            .expect("serve must stop")
            .unwrap()
            .unwrap();
        assert!(
            timeout(Duration::from_millis(100), request).await.is_ok(),
            "timeout must cancel outstanding work"
        );
        return;
    }
    timeout(Duration::from_secs(2), task)
        .await
        .expect("serve must stop")
        .unwrap()
        .unwrap();
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}

#[tokio::test]
async fn drains_request() {
    drain_test(false, false).await;
}
#[tokio::test]
async fn drains_stream() {
    drain_test(true, false).await;
}
#[tokio::test]
async fn shutdown_is_bounded() {
    drain_test(false, true).await;
}

async fn get_on_connection(
    connection: &mut tokio::io::BufReader<tokio::net::TcpStream>,
    path: &str,
) -> (String, String, Vec<u8>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    connection
        .get_mut()
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test-key\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut status = String::new();
    connection.read_line(&mut status).await.unwrap();
    assert!(
        !status.is_empty(),
        "keep-alive client must receive a served response during grace"
    );
    let mut headers = String::new();
    let mut length = None;
    loop {
        let mut line = String::new();
        connection.read_line(&mut line).await.unwrap();
        assert!(!line.is_empty());
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
        headers.push_str(&line.to_ascii_lowercase());
    }
    let mut body = vec![0; length.unwrap()];
    connection.read_exact(&mut body).await.unwrap();
    (status, headers, body)
}

#[tokio::test]
async fn lame_duck_serves_keep_alive_and_new_connections_then_closes_listener() {
    let router = Router::new()
        .route(
            "/healthz",
            get(|| async { ([(http::header::CONTENT_LENGTH, "2")], "ok") }),
        )
        .route(
            "/work",
            get(|headers: http::HeaderMap| async move {
                assert_eq!(headers[http::header::AUTHORIZATION], "Bearer test-key");
                ([(http::header::CONTENT_LENGTH, "6")], "served")
            }),
        );
    let listener = llmux_server::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, shutdown) = oneshot::channel();
    let began = Arc::new(Semaphore::new(0));
    let started = began.clone();
    let task = tokio::spawn(llmux_server::serve(
        listener,
        router,
        async move {
            let _ = shutdown.await;
            started.add_permits(1);
        },
        Duration::from_secs(1),
        Duration::from_secs(1),
    ));
    let mut existing =
        tokio::io::BufReader::new(tokio::net::TcpStream::connect(address).await.unwrap());
    assert!(get_on_connection(&mut existing, "/work")
        .await
        .0
        .contains("200"));
    stop.send(()).unwrap();
    began.acquire().await.unwrap().forget();
    let (status, headers, body) = get_on_connection(&mut existing, "/work").await;
    assert!(status.contains("200"));
    assert_eq!(body, b"served");
    assert!(headers.contains("connection: close\r\n"));
    let mut fresh = tokio::io::BufReader::new(
        tokio::net::TcpStream::connect(address)
            .await
            .expect("new connections must be accepted during grace"),
    );
    let (status, headers, body) = get_on_connection(&mut fresh, "/work").await;
    assert!(status.contains("200"));
    assert_eq!(body, b"served");
    assert!(headers.contains("connection: close\r\n"));
    let mut health =
        tokio::io::BufReader::new(tokio::net::TcpStream::connect(address).await.unwrap());
    let (status, headers, _) = get_on_connection(&mut health, "/healthz").await;
    assert!(status.contains("503"));
    assert!(headers.contains("connection: close\r\n"));
    assert!(
        !task.is_finished(),
        "serve must remain alive for the grace period"
    );
    timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}
