//! Client side of the agent IPC.
//!
//! Connects to a running session, sends a command, and blocks until the
//! session returns output + exit code.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::{socket_path, KILL_CMD};

/// Connect to a running session and execute one command.
/// Returns `(stdout+stderr output, exit_code)`.
pub async fn send_command(session_id: &str, cmd: &str) -> anyhow::Result<(String, i32)> {
    let path = socket_path(session_id);
    let mut stream = UnixStream::connect(&path).await.map_err(|_| {
        anyhow::anyhow!("session '{session_id}' not found — is kash listening?")
    })?;

    stream.write_all(format!("{cmd}\n").as_bytes()).await?;

    let mut bytes = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
    }

    let all = String::from_utf8_lossy(&bytes);

    if let Some(pos) = all.rfind('\x00') {
        let output = all[..pos].to_string();
        let exit_code = all[pos + 1..]
            .strip_prefix("SHEX:")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        Ok((output, exit_code))
    } else {
        Ok((all.into_owned(), 0))
    }
}

/// Send a graceful-kill signal to a running session.
pub async fn kill_session(session_id: &str) -> anyhow::Result<()> {
    send_command(session_id, KILL_CMD).await?;
    Ok(())
}
