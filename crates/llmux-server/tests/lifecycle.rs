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
