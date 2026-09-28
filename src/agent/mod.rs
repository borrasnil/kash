//! Agent IPC: Unix-socket communication between the interactive session
//! and external callers (agents, LLMs).
//!
//! * [`server`] — spawned inside the interactive session; listens on a Unix
//!   socket and forwards commands to the session loop.
//! * [`client`] — connects to a running session, sends a command, and blocks
//!   until the session returns output + exit code.
//!
//! Protocol (one command per connection):
//! ```text
//! client → server   <command text>\n
//! server → client   <stdout/stderr bytes>
//!                   \x00SHEX:<exit_code>\n   (trailer, never appears in real output)
//! ```
//!
//! Special commands:
//!   `__SHHANDLER_KILL__`  — signals the session to terminate gracefully

mod client;
mod server;

pub use client::{kill_session, send_command};
pub use server::serve;

use tokio::sync::oneshot;



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

/// Sent as the command string to request an interactive attach.
/// After sending this, the connection switches to bidirectional byte relay.
pub const ATTACH_CMD: &str = "__SHHANDLER_ATTACH__";

/// IPC prefix for upload-via-exec: `"__SHHANDLER_UPLOAD__\x00{local}\x00{remote}"`.
/// Paths are separated by NUL bytes (valid in &str, not a line terminator).
pub const UPLOAD_CMD_PREFIX: &str = "__SHHANDLER_UPLOAD__\x00";

/// IPC prefix for download-via-exec: `"__SHHANDLER_DOWNLOAD__\x00{remote}\x00{local}"`.
pub const DOWNLOAD_CMD_PREFIX: &str = "__SHHANDLER_DOWNLOAD__\x00";

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
