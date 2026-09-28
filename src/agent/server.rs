//! Server side of the agent IPC.
//!
//! Spawned inside the interactive session; listens on a Unix socket and
//! forwards incoming commands to the session loop via an mpsc channel.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use super::{AgentCommand, ATTACH_CMD};

/// Spawn a background Unix-socket listener for the given session.
///
/// Incoming connections each deliver one `AgentCommand` to `cmd_tx`, except
/// for `ATTACH_CMD` connections which are forwarded to `attach_tx` as a live
/// `UnixStream` for bidirectional byte relay.
pub fn serve(
    session_id: &str,
    cmd_tx: mpsc::Sender<AgentCommand>,
    attach_tx: mpsc::Sender<UnixStream>,
) -> std::io::Result<()> {
    let path = super::socket_path(session_id);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tx = cmd_tx.clone();
                    let atx = attach_tx.clone();
                    tokio::spawn(handle_conn(stream, tx, atx));
                }
                Err(_) => break,
            }
        }
    });

    Ok(())
}

async fn handle_conn(
    stream: UnixStream,
    cmd_tx: mpsc::Sender<AgentCommand>,
    attach_tx: mpsc::Sender<UnixStream>,
) {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();

    if reader.read_line(&mut line).await.is_err() {
        return;
    }
    let cmd = line.trim_end_matches(['\n', '\r']).to_string();
    if cmd.is_empty() {
        return;
    }

    if cmd == ATTACH_CMD {
        // Reunite the halves and hand the live stream to the session loop.
        let r = reader.into_inner();
        if let Ok(stream) = r.reunite(w) {
            let _ = attach_tx.send(stream).await;
        }
        return;
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    if cmd_tx.send(AgentCommand { cmd, response_tx: tx }).await.is_err() {
        return;
    }

    if let Ok(resp) = rx.await {
        let _ = w.write_all(resp.output.as_bytes()).await;
        let trailer = format!("\x00SHEX:{}\n", resp.exit_code);
        let _ = w.write_all(trailer.as_bytes()).await;
    }
}
