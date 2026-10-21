//! Session state structures: cleanup guard, live metadata, display state,
//! and the session state machine enum.

use crate::agent;
use crate::cli::ShellType;
use crate::prompt;
use crate::transport::TransportInfo;

// ---------------------------------------------------------------------------
// RAII cleanup guard
// ---------------------------------------------------------------------------

pub(super) struct CleanupGuard {
    sock_path: String,
    meta_path: String,
}

impl CleanupGuard {
    pub(super) fn new(session_id: &str) -> Self {
        Self {
            sock_path: agent::socket_path(session_id),
            meta_path: agent::info_path(session_id),
        }
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock_path);
        let _ = std::fs::remove_file(&self.meta_path);
    }
}

// ---------------------------------------------------------------------------
// Live session metadata
// ---------------------------------------------------------------------------

pub(super) struct LiveMeta {
    pub session_id: String,
    pub peer: String,
    pub user: String,
    pub host: String,
    pub transport: &'static str,
    pub shell: &'static str,
    pub started: u64,
    pub last_cmd: String,
    pub last_cmd_at: u64,
    pub cmd_count: u32,
}

impl LiveMeta {
    pub fn new(
        session_id: &str,
        transport: &TransportInfo,
        user: &str,
        host: &str,
        shell_type: ShellType,
    ) -> Self {
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            session_id: session_id.to_string(),
            peer: transport.peer.clone(),
            user: user.to_string(),
            host: host.to_string(),
            transport: transport.kind.as_str(),
            shell: prompt::shell_label(shell_type),
            started,
            last_cmd: String::new(),
            last_cmd_at: 0,
            cmd_count: 0,
        }
    }

    pub fn record_cmd(&mut self, cmd: &str) {
        self.last_cmd = cmd.trim().replace('\n', " ");
        self.last_cmd_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.cmd_count += 1;
        self.persist();
    }

    pub fn persist(&self) {
        let content = format!(
            "peer={}\nuser={}\nhost={}\ntransport={}\nshell={}\n\
             started={}\nlast_cmd={}\nlast_cmd_at={}\ncmd_count={}\n",
            self.peer,
            self.user,
            self.host,
            self.transport,
            self.shell,
            self.started,
            self.last_cmd,
            self.last_cmd_at,
            self.cmd_count,
        );
        let _ = std::fs::write(agent::info_path(&self.session_id), content);
    }
}

// ---------------------------------------------------------------------------
// Display state — tracks multi-row input so it can be fully erased on redraw
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
pub(super) struct DisplayState {
    /// Row the cursor is on within the prompt+buffer display (0 = top).
    pub cursor_row: u16,
}

// ---------------------------------------------------------------------------
// Session state machine
// ---------------------------------------------------------------------------

pub(super) enum SessionState {
    Interactive,
    AgentCollecting {
        nonce: String,
        buffer: String,
        response_tx: tokio::sync::oneshot::Sender<crate::agent::AgentResponse>,
        /// True when the session is in raw PTY mode at command injection time.
        /// The PTY line-discipline echoes the injected command back as the first
        /// line of output; this flag tells the extractor to skip that line.
        pty_mode: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ShellType;
    use crate::transport::{TransportCaps, TransportKind};

    #[test]
    fn live_meta_record_cmd_increments() {
        let info = TransportInfo::new(
            TransportKind::Ssh,
            "127.0.0.1:9999",
            TransportCaps::pty(),
        );
        let mut m = LiveMeta::new("__test_meta__", &info, "root", "box", ShellType::Linux);
        assert_eq!(m.transport, "ssh");
        assert_eq!(m.cmd_count, 0);
        m.record_cmd("ls");
        assert_eq!(m.cmd_count, 1);
        m.record_cmd("whoami");
        assert_eq!(m.cmd_count, 2);
        let _ = std::fs::remove_file(agent::info_path("__test_meta__"));
    }

    #[test]
    fn live_meta_strips_newlines() {
        let info = TransportInfo::new(
            TransportKind::Reverse,
            "127.0.0.1:9999",
            TransportCaps::reverse(),
        );
        let mut m = LiveMeta::new("__test_meta2__", &info, "u", "h", ShellType::Auto);
        m.record_cmd("echo\nhello");
        assert!(!m.last_cmd.contains('\n'));
        let _ = std::fs::remove_file(agent::info_path("__test_meta2__"));
    }
}
