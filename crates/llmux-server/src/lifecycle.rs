use axum::Router;
use axum::{
    extract::Request,
    middleware::{self, Next},
    response::IntoResponse,
    serve::ListenerExt,
};
use futures_util::StreamExt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::{
    future::{Future, IntoFuture},
    io,
    net::SocketAddr,
    time::Duration,
};
use tokio::net::{TcpListener, TcpSocket};

pub async fn bind(address: SocketAddr) -> io::Result<TcpListener> {
    let socket = if address.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    #[cfg(unix)]
    socket.set_reuseport(true)?;
    socket.bind(address)?;
    socket.listen(1024)
}

fn with_shutdown(
    app: Router,
    state: Arc<AtomicBool>,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Router {
    app.layer(middleware::from_fn(move |request: Request, next: Next| {
        let state = state.clone();
        let mut cancel = cancel.clone();
        async move {
            let http1 = matches!(request.version(), http::Version::HTTP_10 | http::Version::HTTP_11);
            let mut response = if request.uri().path() == "/healthz" && state.load(Ordering::Acquire) {
                (http::StatusCode::SERVICE_UNAVAILABLE, "draining").into_response()
            } else {
                let response = tokio::select! {
                    response = next.run(request) => response,
                    _ = cancel.changed() => return (http::StatusCode::SERVICE_UNAVAILABLE, "shutdown timeout").into_response(),
                };
                let (parts, body) = response.into_parts();
                let stream = body.into_data_stream().take_until(async move { let _ = cancel.changed().await; });
                axum::response::Response::from_parts(parts, axum::body::Body::from_stream(stream))
            };
            if http1 && state.load(Ordering::Acquire) {
                response.headers_mut().insert(http::header::CONNECTION, http::HeaderValue::from_static("close"));
            }
            response
        }
    }))
}

pub async fn serve(
    listener: TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    shutdown_timeout: Duration,
    drain_grace: Duration,
) -> io::Result<()> {
    #[cfg(unix)]
    let notify_socket = std::env::var_os("NOTIFY_SOCKET").map(std::path::PathBuf::from);
    #[cfg(unix)]
    let watchdog = if let Some(socket) = &notify_socket {
        crate::notify::send(socket, "READY=1")?;
        let address = listener.local_addr()?;
        std::env::var("WATCHDOG_USEC")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value >= 3)
            .map(|usec| {
                let socket = socket.clone();
                tokio::spawn(async move {
                    crate::notify::watchdog(&socket, address, Duration::from_micros(usec / 3)).await
                })
            })
    } else {
        None
    };
    let draining = Arc::new(AtomicBool::new(false));
    let (force, cancel) = tokio::sync::watch::channel(false);
    let app = with_shutdown(app, draining.clone(), cancel);
    let (began, started) = tokio::sync::oneshot::channel();
    let graceful = axum::serve(
        listener.tap_io(|tcp| {
            let _ = tcp.set_nodelay(true);
        }),
        app,
    )
    .with_graceful_shutdown(async move {
        shutdown.await;
        draining.store(true, Ordering::Release);
        #[cfg(unix)]
        {
            if let Some(task) = watchdog {
                task.abort();
            }
            if let Some(socket) = notify_socket {
                let _ = crate::notify::send(&socket, "STOPPING=1");
            }
        }
        if !drain_grace.is_zero() {
            tokio::time::sleep(drain_grace).await;
        }
        let _ = began.send(());
    })
    .into_future();
    tokio::pin!(graceful);
    tokio::select! {
        result = &mut graceful => result,
        _ = started => {
            match tokio::time::timeout(shutdown_timeout, graceful).await {
                Ok(result) => result,
                Err(_) => {
                    let _ = force.send(true);
                    Ok(())
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use http::{header, HeaderMap, StatusCode};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    async fn request(connection: &mut BufReader<tokio::net::TcpStream>, path: &str) -> String {
        connection.get_mut().write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test-key\r\n\r\n").as_bytes()).await.unwrap();
        let mut status = String::new();
        connection.read_line(&mut status).await.unwrap();
        let mut length = None;
        loop {
            let mut line = String::new();
            connection.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
            assert!(
                !line.is_empty(),
                "connection closed before response headers"
            );
        }
        let mut body = vec![0; length.unwrap()];
        connection.read_exact(&mut body).await.unwrap();
        status
    }

    #[tokio::test]
    async fn draining_only_rejects_health_on_existing_keep_alive_connection() {
        let draining = Arc::new(AtomicBool::new(false));
        let (_force, cancel) = tokio::sync::watch::channel(false);
        let router = Router::new()
            .route(
                "/healthz",
                get(|| async { ([(header::CONTENT_LENGTH, "2")], "ok") }),
            )
            .route(
                "/work",
                get(|headers: HeaderMap| async move {
                    assert!(crate::authorized(&headers, &[Arc::from("test-key")]));
                    ([(header::CONTENT_LENGTH, "6")], "served")
                }),
            );
        let server =
            llmux_testkit::TestServer::start(with_shutdown(router, draining.clone(), cancel))
                .await
                .unwrap();
        let address = server.url().trim_start_matches("http://").to_owned();
        let mut connection = BufReader::new(tokio::net::TcpStream::connect(address).await.unwrap());
        assert!(request(&mut connection, "/healthz").await.contains("200"));
        draining.store(true, Ordering::Release);
        assert!(
            request(&mut connection, "/work").await.contains("200"),
            "authorized work must survive drain admission"
        );
        let address = server.url().trim_start_matches("http://").to_owned();
        let mut connection = BufReader::new(tokio::net::TcpStream::connect(address).await.unwrap());
        let status = request(&mut connection, "/healthz").await;
        assert!(status.contains(StatusCode::SERVICE_UNAVAILABLE.as_str()));
    }
}
