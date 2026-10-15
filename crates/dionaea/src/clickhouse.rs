use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RESPONSE: u64 = 1024;

/// Asks ClickHouse's `/ping` endpoint whether it is up. Bounded in time and
/// in bytes read; a down server is reported, never fatal.
pub(crate) async fn ping(host: &str, port: u16) -> Result<(), String> {
    tokio::time::timeout(TIMEOUT, ping_inner(host, port))
        .await
        .map_err(|_| "timed out".to_owned())?
}

async fn ping_inner(host: &str, port: u16) -> Result<(), String> {
    let mut stream = TcpStream::connect((host, port))
        .await
        .map_err(|e| e.to_string())?;
    let request = format!("GET /ping HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    stream
        .take(MAX_RESPONSE)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| e.to_string())?;
    let head = String::from_utf8_lossy(&buf);
    if head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        Err(head.lines().next().unwrap_or("empty response").to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn serve_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let _ = socket.read(&mut buf).await;
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        port
    }

    #[tokio::test]
    async fn ok_response_is_reachable() {
        let port = serve_once("HTTP/1.1 200 OK\r\n\r\nOk.\n").await;
        assert_eq!(ping("127.0.0.1", port).await, Ok(()));
    }

    #[tokio::test]
    async fn error_status_is_reported() {
        let port = serve_once("HTTP/1.1 503 Service Unavailable\r\n\r\n").await;
        assert!(ping("127.0.0.1", port).await.unwrap_err().contains("503"));
    }

    #[tokio::test]
    async fn closed_port_is_reported() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(ping("127.0.0.1", port).await.is_err());
    }
}
