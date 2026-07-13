// Unix-socket IPC between the interactive session and external callers (agents, LLMs).
//
// Interactive session:  `serve()` spawns a background listener; incoming commands
//                       arrive as `AgentCommand` on an mpsc channel.
//
// External caller:      `send_command()` connects, sends a command, and blocks
//                       until the session returns output + exit code.
//
// Protocol (one command per connection):
//   client → server   <command text>\n
//   server → client   <stdout/stderr bytes>
//                     \x00SHEX:<exit_code>\n   (trailer, never appears in real output)
//
// Special commands:
//   __SHHANDLER_KILL__  — signals the session to terminate gracefully

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

pub struct AgentCommand {
    pub cmd: String,
    pub response_tx: oneshot::Sender<AgentResponse>,
}

pub struct AgentResponse {
    pub output: String,
    pub exit_code: i32,
}

/// Sent as the command string to signal the session to exit.
pub const KILL_CMD: &str = "__SHHANDLER_KILL__";

// ---------------------------------------------------------------------------
// Session metadata (written to /tmp/.shh-<id>.info, read by `ps`/`inspect`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub peer: String,
    pub user: String,
    pub host: String,
    pub obfuscation: String,
    pub shell: String,
    /// Unix epoch of session start (0 = unknown).
    pub started: u64,
    /// Last command submitted (empty = none yet).
    pub last_cmd: String,
    /// Unix epoch when last_cmd was submitted (0 = unknown).
    pub last_cmd_at: u64,
    /// Total commands run in this session.
    pub cmd_count: u32,
}

/// Return all currently-active sessions by scanning /tmp for socket files.
pub fn list_sessions() -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir("/tmp") else {
        return vec![];
    };
    let mut sessions: Vec<SessionInfo> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy().into_owned();
            if name.starts_with(".shh-") && name.ends_with(".sock") {
                let id = &name[5..name.len() - 5];
                Some(read_session_info(id))
            } else {
                None
            }
        })
        .collect();
    sessions.sort_by(|a, b| a.id.cmp(&b.id));
    sessions
}

/// Read the `.info` file for a session; missing fields come back as `"?"`.
pub fn read_session_info(id: &str) -> SessionInfo {
    let mut info = SessionInfo {
        id: id.to_string(),
        peer: "?".to_string(),
        user: "?".to_string(),
        host: "?".to_string(),
        obfuscation: "?".to_string(),
        shell: "?".to_string(),
        started: 0,
        last_cmd: String::new(),
        last_cmd_at: 0,
        cmd_count: 0,
    };
    let path = info_path(id);
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k {
                    "peer" => info.peer = v.to_string(),
                    "user" => info.user = v.to_string(),
                    "host" => info.host = v.to_string(),
                    "obfuscation" => info.obfuscation = v.to_string(),
                    "shell" => info.shell = v.to_string(),
                    "started" => info.started = v.parse().unwrap_or(0),
                    "last_cmd" => info.last_cmd = v.to_string(),
                    "last_cmd_at" => info.last_cmd_at = v.parse().unwrap_or(0),
                    "cmd_count" => info.cmd_count = v.parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
    }
    info
}

// ---------------------------------------------------------------------------
// Filesystem paths
// ---------------------------------------------------------------------------

pub fn socket_path(session_id: &str) -> String {
    format!("/tmp/.shh-{session_id}.sock")
}

pub fn info_path(session_id: &str) -> String {
    format!("/tmp/.shh-{session_id}.info")
}

// ---------------------------------------------------------------------------
// Server side (spawned inside the interactive session)
// ---------------------------------------------------------------------------

/// Spawn a background Unix-socket listener for the given session.
/// Incoming connections each deliver one `AgentCommand` to `cmd_tx`.
pub fn serve(session_id: &str, cmd_tx: mpsc::Sender<AgentCommand>) -> std::io::Result<()> {
    let path = socket_path(session_id);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tx = cmd_tx.clone();
                    tokio::spawn(handle_conn(stream, tx));
                }
                Err(_) => break,
            }
        }
    });

    Ok(())
}

async fn handle_conn(stream: UnixStream, cmd_tx: mpsc::Sender<AgentCommand>) {
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

    let (tx, rx) = oneshot::channel();
    if cmd_tx.send(AgentCommand { cmd, response_tx: tx }).await.is_err() {
        return;
    }

    if let Ok(resp) = rx.await {
        let _ = w.write_all(resp.output.as_bytes()).await;
        let trailer = format!("\x00SHEX:{}\n", resp.exit_code);
        let _ = w.write_all(trailer.as_bytes()).await;
    }
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

/// Connect to a running session and execute one command.
/// Returns `(stdout+stderr output, exit_code)`.
pub async fn send_command(session_id: &str, cmd: &str) -> anyhow::Result<(String, i32)> {
    let path = socket_path(session_id);
    let mut stream = UnixStream::connect(&path).await.map_err(|_| {
        anyhow::anyhow!("session '{session_id}' not found — is shell-handler listening?")
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_and_info_paths() {
        let s = socket_path("abc12345");
        let i = info_path("abc12345");
        assert_eq!(s, "/tmp/.shh-abc12345.sock");
        assert_eq!(i, "/tmp/.shh-abc12345.info");
    }

    #[test]
    fn read_session_info_missing_returns_question_marks() {
        let info = read_session_info("__no_such_session_xyz__");
        assert_eq!(info.peer, "?");
        assert_eq!(info.user, "?");
        assert_eq!(info.host, "?");
        assert_eq!(info.started, 0);
        assert_eq!(info.cmd_count, 0);
    }

    #[test]
    fn read_session_info_partial_file() {
        let id = "__test_info_partial__";
        let path = info_path(id);
        std::fs::write(&path, "user=testuser\nhost=box\n").unwrap();
        let info = read_session_info(id);
        let _ = std::fs::remove_file(&path);
        assert_eq!(info.user, "testuser");
        assert_eq!(info.host, "box");
        assert_eq!(info.peer, "?");
    }

    #[test]
    fn list_sessions_empty_when_no_sockets() {
        // Just verifies it doesn't panic; real sockets may or may not be present.
        let _ = list_sessions();
    }

    #[test]
    fn kill_cmd_constant() {
        assert!(!KILL_CMD.is_empty());
        assert!(KILL_CMD.starts_with("__SHHANDLER"));
    }
}
