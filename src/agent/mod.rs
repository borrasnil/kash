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

/// One inbound IPC command plus the channel to answer it on. Created by the
/// socket listener in [`server`] and consumed by the session loop; the
/// command string is one of the `__SHHANDLER_*` prefixed special forms or a
/// plain command to run on the remote shell.
pub struct AgentCommand {
    /// Raw command line (terminated form stripped by the server).
    pub cmd: String,
    pub response_tx: oneshot::Sender<AgentResponse>,
}

/// Answer sent back over the IPC connection: collected output plus the
/// command's exit code (negative/special values encode cancellation and
/// timeouts — see the session loop's `cancel_agent` and watchdog).
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

/// IPC prefix for module scripts: `"__SHHANDLER_RUN__\x00{display}\x00{cmd}"`.
///
/// `{cmd}` is a single line run **verbatim**.
/// `{display}` (e.g. `run enum-users`) is shown in the terminal banner and
/// recorded in session history instead of the raw payload.
pub const RUN_CMD_PREFIX: &str = "__SHHANDLER_RUN__\x00";

// ---------------------------------------------------------------------------
// Session metadata (written to /tmp/.shh-<id>.info, read by `ps`/`inspect`)
// ---------------------------------------------------------------------------

/// One active session, parsed from its `/tmp/.shh-<id>.info` metadata file
/// (`"?"` for fields written by unknown/older binaries).
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub peer: String,
    pub user: String,
    pub host: String,
    /// How the session was established (`reverse` | `ssh` | `winrm`;
    /// `"?"` for sessions started by older binaries).
    pub transport: String,
    pub shell: String,
    /// Unix epoch of session start (0 = unknown).
    pub started: u64,
    /// Last command submitted (empty = none yet).
    pub last_cmd: String,
    /// Unix epoch when last_cmd was submitted (0 = unknown).
    pub last_cmd_at: u64,
    /// Total commands run in this session.
    pub cmd_count: u32,
    /// Serving process pid (0 = unknown, older binaries or foreign files).
    pub pid: u32,
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

/// Live-probe one session by connecting to its socket. A socket file without
/// a serving process (crash, reboot) refuses the connect — dead.
pub async fn session_alive(id: &str) -> bool {
    match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        tokio::net::UnixStream::connect(socket_path(id)),
    )
    .await
    {
        Ok(Ok(_)) => true,
        _ => false,
    }
}

/// [`list_sessions`] with a liveness check. Sessions whose socket no longer
/// has a serving process are pruned (`rm` of the stale `.sock`/`.info`
/// leftovers) and dropped, so `ps` only shows sessions that can actually be
/// attached/exec'd. The probe is a local unix-socket connect — sub-ms for
/// live sessions, instant `ECONNREFUSED` for dead ones.
pub async fn list_sessions_alive() -> Vec<SessionInfo> {
    let mut out = Vec::new();
    for s in list_sessions() {
        if session_alive(&s.id).await {
            out.push(s);
        } else {
            let _ = std::fs::remove_file(socket_path(&s.id));
            let _ = std::fs::remove_file(info_path(&s.id));
        }
    }
    out
}

/// Best-effort `SIGCONT` to the session's serving process. A session left
/// SIGSTOPed (detach before the documented `bg`) looks live to a probe —
/// the listener accepts — but relays nothing: attach shows a banner over a
/// frozen stream and RPC hangs. Resuming on connect makes both recover.
/// Harmless no-op for running sessions, unix-only, silent on errors.
pub fn rescue_stopped(session_id: &str) {
    #[cfg(unix)]
    {
        let pid = read_session_info(session_id).pid;
        if pid > 0 {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGCONT);
            }
        }
    }
}

/// Read the `.info` file for a session; missing fields come back as `"?"`.
pub fn read_session_info(id: &str) -> SessionInfo {
    let mut info = SessionInfo {
        id: id.to_string(),
        peer: "?".to_string(),
        user: "?".to_string(),
        host: "?".to_string(),
        transport: "?".to_string(),
        shell: "?".to_string(),
        started: 0,
        last_cmd: String::new(),
        last_cmd_at: 0,
        cmd_count: 0,
        pid: 0,
    };
    let path = info_path(id);
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k {
                    "peer" => info.peer = v.to_string(),
                    "user" => info.user = v.to_string(),
                    "host" => info.host = v.to_string(),
                    "transport" => info.transport = v.to_string(),
                    "shell" => info.shell = v.to_string(),
                    "started" => info.started = v.parse().unwrap_or(0),
                    "last_cmd" => info.last_cmd = v.to_string(),
                    "last_cmd_at" => info.last_cmd_at = v.parse().unwrap_or(0),
                    "cmd_count" => info.cmd_count = v.parse().unwrap_or(0),
                    "pid" => info.pid = v.parse().unwrap_or(0),
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

/// Unix socket path for a session (`/tmp/.shh-<id>.sock`).
///
/// The presence of this file is itself the session registry: `ps` scans for
/// it, and `attach`/`exec` connect to it directly.
pub fn socket_path(session_id: &str) -> String {
    format!("/tmp/.shh-{session_id}.sock")
}

/// Path of the key=value metadata file written by the session loop
/// (see [`SessionInfo`] for the field list).
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
