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

pub async fn serve(
    listener: TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    shutdown_timeout: Duration,
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
    let state = draining.clone();
    let app = app.layer(middleware::from_fn(move |request: Request, next: Next| {
        let state = state.clone();
        let mut cancel = cancel.clone();
        async move {
            if state.load(Ordering::Acquire) {
                (http::StatusCode::SERVICE_UNAVAILABLE, "draining").into_response()
            } else {
                let response = tokio::select! {
                    response = next.run(request) => response,
                    _ = cancel.changed() => return (http::StatusCode::SERVICE_UNAVAILABLE, "shutdown timeout").into_response(),
                };
                let (parts, body) = response.into_parts();
                let stream = body.into_data_stream().take_until(async move { let _ = cancel.changed().await; });
                axum::response::Response::from_parts(parts, axum::body::Body::from_stream(stream))
            }
        }
    }));
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
