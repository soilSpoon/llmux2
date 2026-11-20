use std::{io, net::SocketAddr, path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub fn send(socket: &Path, message: &str) -> io::Result<()> {
    use std::os::unix::{ffi::OsStrExt, net::UnixDatagram};
    let datagram = UnixDatagram::unbound()?;
    datagram.set_nonblocking(true)?;
    let bytes = socket.as_os_str().as_bytes();
    if bytes.first() == Some(&b'@') {
        #[cfg(target_os = "linux")]
        {
            use std::os::linux::net::SocketAddrExt;
            let address = std::os::unix::net::SocketAddr::from_abstract_name(&bytes[1..])?;
            datagram.connect_addr(&address)?;
        }
        #[cfg(not(target_os = "linux"))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "abstract notify socket",
        ));
    } else {
        datagram.connect(socket)?;
    }
    datagram.send(message.as_bytes())?;
    Ok(())
}

async fn probe(mut address: SocketAddr) -> io::Result<bool> {
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    let mut stream = tokio::net::TcpStream::connect(address).await?;
    stream
        .write_all(
            format!("GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut line = Vec::with_capacity(64);
    while line.len() < 256 {
        let byte = stream.read_u8().await?;
        line.push(byte);
        if byte == b'\n' {
            break;
        }
    }
    Ok(line.starts_with(b"HTTP/1.1 200 ") || line.starts_with(b"HTTP/1.0 200 "))
}

pub async fn watchdog(socket: &Path, address: SocketAddr, period: Duration) {
    loop {
        tokio::time::sleep(period).await;
        if matches!(
            tokio::time::timeout(period.min(Duration::from_secs(2)), probe(address)).await,
            Ok(Ok(true))
        ) {
            let _ = send(socket, "WATCHDOG=1");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use llmux_testkit::TestServer;
    use tokio::{net::UnixDatagram, time::timeout};

    #[tokio::test]
    async fn ready_is_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let receiver = UnixDatagram::bind(&path).unwrap();
        std::env::set_var("NOTIFY_SOCKET", &path);
        let listener = crate::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let (stop, shutdown) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(crate::serve(
            listener,
            Router::new(),
            async {
                let _ = shutdown.await;
            },
            Duration::from_secs(1),
        ));
        let mut buffer = [0; 64];
        let count = timeout(Duration::from_millis(100), receiver.recv(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buffer[..count], b"READY=1");
        std::env::remove_var("NOTIFY_SOCKET");
        stop.send(()).unwrap();
        let count = timeout(Duration::from_secs(1), receiver.recv(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buffer[..count], b"STOPPING=1");
        task.await.unwrap().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn abstract_socket_is_supported() {
        use std::os::linux::net::SocketAddrExt;
        let name = format!("llmux-notify-{}", uuid::Uuid::new_v4());
        let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let receiver = std::os::unix::net::UnixDatagram::bind_addr(&address).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        send(Path::new(&format!("@{name}")), "READY=1").unwrap();
        let mut buffer = [0; 64];
        let count = receiver.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..count], b"READY=1");
    }

    #[tokio::test]
    async fn watchdog_requires_healthy_http() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let receiver = UnixDatagram::bind(&path).unwrap();
        let server = TestServer::start(Router::new().route("/healthz", get(|| async { "ok" })))
            .await
            .unwrap();
        let address = server.url().trim_start_matches("http://").parse().unwrap();
        let socket = path.clone();
        let task =
            tokio::spawn(
                async move { watchdog(&socket, address, Duration::from_millis(10)).await },
            );
        let mut buffer = [0; 64];
        let count = timeout(Duration::from_secs(1), receiver.recv(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buffer[..count], b"WATCHDOG=1");
        task.abort();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task =
            tokio::spawn(async move { watchdog(&path, address, Duration::from_millis(10)).await });
        assert!(
            timeout(Duration::from_millis(150), receiver.recv(&mut buffer))
                .await
                .is_err()
        );
        task.abort();
    }
}
