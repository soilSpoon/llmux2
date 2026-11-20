//! Loopback fixtures for tests exercising the real gateway over HTTP.

pub mod mock_load_upstream;

use axum::Router;
use std::{io, net::SocketAddr};
use tokio::{net::TcpListener, task::JoinHandle};

pub struct TestServer {
    address: SocketAddr,
    task: JoinHandle<io::Result<()>>,
}

impl TestServer {
    pub async fn start(router: Router) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let task = tokio::spawn(async move { axum::serve(listener, router).await });
        Ok(Self { address, task })
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
