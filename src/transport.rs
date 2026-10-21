//! Remote-shell transports.
//!
//! Every way kash reaches a shell — inbound reverse shells (`listen`),
//! outbound SSH (`ssh`), WinRM (`winrm`, Phase 2), and future transports
//! (Kubernetes/Docker exec) — implements [`Transport`]: establish the
//! connection and hand back split stream halves plus a [`TransportInfo`].
//!
//! The session loop ([`crate::session::run_session`]) never matches on
//! *which* transport it serves. It branches only on [`TransportCaps`]:
//!
//! * [`TransportCaps::has_pty`] — the remote end can do raw PTY passthrough.
//!   Without it the session stays in handler/line-editor mode.
//! * [`TransportCaps::starts_with_pty`] — the transport arrives with a
//!   working PTY (SSH, K8s). When false but `has_pty` is true (reverse
//!   shells), the session sends the `pty.spawn` upgrade first.
//! * [`TransportCaps::supports_resize`] — terminal resizes are forwarded
//!   via `stty`.
//!
//! # Example
//!
//! ```
//! use kash::transport::{TransportCaps, TransportInfo, TransportKind};
//!
//! let info = TransportInfo::new(
//!     TransportKind::Ssh,
//!     "root@10.0.0.5:22",
//!     TransportCaps::pty(),
//! );
//! assert_eq!(info.kind.as_str(), "ssh");
//! assert!(info.caps.has_pty);
//! ```

use tokio::io::{AsyncRead, AsyncWrite};

/// Boxed shell-stream halves. Connectors erase their concrete stream types
/// (`TcpStream` halves, russh channel halves, …) into these so every
/// transport feeds [`crate::session::run_session`] identically.
pub type BoxReader = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWriter = Box<dyn AsyncWrite + Unpin + Send>;

/// How a shell session was established. Displayed in `ps`/`inspect` via
/// [`TransportKind::as_str`]; new transports add a variant here and set
/// their [`TransportCaps`] — the session loop itself needs no changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportKind {
    /// Inbound reverse shell (`kash listen`).
    #[default]
    Reverse,
    /// Outbound SSH (`kash ssh`).
    Ssh,
    /// Windows Remote Management (`kash winrm`).
    Winrm,
}

impl TransportKind {
    /// Short machine-friendly label (`reverse` | `ssh` | `winrm`).
    ///
    /// ```
    /// use kash::transport::TransportKind;
    /// assert_eq!(TransportKind::Winrm.as_str(), "winrm");
    /// ```
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reverse => "reverse",
            Self::Ssh => "ssh",
            Self::Winrm => "winrm",
        }
    }
}

/// What the session loop may assume about a transport. Construct with one
/// of the named constructors — never literal-struct it at call sites, so
/// new capabilities get sane defaults everywhere.
///
/// ```
/// use kash::transport::TransportCaps;
///
/// let ssh = TransportCaps::pty();
/// assert!(ssh.has_pty && ssh.starts_with_pty && ssh.supports_resize);
///
/// let winrm = TransportCaps::no_pty();
/// assert!(!winrm.has_pty && !winrm.starts_with_pty);
///
/// let reverse = TransportCaps::reverse();
/// assert!(reverse.has_pty && !reverse.starts_with_pty);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportCaps {
    /// Raw PTY passthrough is usable (possibly after an upgrade step).
    pub has_pty: bool,
    /// The transport arrives with a working PTY; no `pty.spawn` needed.
    pub starts_with_pty: bool,
    /// Terminal resizes are forwarded (`stty cols/rows`).
    pub supports_resize: bool,
}

impl TransportCaps {
    /// Full PTY transports that arrive ready: SSH, Kubernetes/Docker exec.
    pub fn pty() -> Self {
        Self {
            has_pty: true,
            starts_with_pty: true,
            supports_resize: true,
        }
    }

    /// Reverse shells: PTY-capable, but kash must upgrade first.
    pub fn reverse() -> Self {
        Self {
            has_pty: true,
            starts_with_pty: false,
            supports_resize: true,
        }
    }

    /// Command-channel transports with no PTY: WinRM, SMB-exec.
    /// Sessions stay in handler/line-editor mode.
    pub fn no_pty() -> Self {
        Self {
            has_pty: false,
            starts_with_pty: false,
            supports_resize: false,
        }
    }
}

/// Identity + capabilities of an established transport, built by the
/// connector and handed to the session loop alongside the streams.
#[derive(Debug, Clone)]
pub struct TransportInfo {
    /// Which transport established the session (shown in `ps`/`inspect`).
    pub kind: TransportKind,
    /// Human-readable remote address (`192.168.1.5:4444`, `root@h:22`).
    pub peer: String,
    /// What the session loop may assume (see [`TransportCaps`]).
    pub caps: TransportCaps,
    /// Command dialect spoken by the remote shell. Drives marker framing
    /// and identity probing in the session loop (see [`RemoteShell`]).
    /// Defaults to [`RemoteShell::Posix`]; connectors override via
    /// [`TransportInfo::with_shell`].
    pub shell: RemoteShell,
}

impl TransportInfo {
    /// ```
    /// use kash::transport::{TransportCaps, TransportInfo, TransportKind};
    ///
    /// let info = TransportInfo::new(TransportKind::Reverse, "10.0.0.5:4444", TransportCaps::reverse());
    /// assert_eq!(info.peer, "10.0.0.5:4444");
    /// assert!(!info.caps.starts_with_pty);
    /// ```
    pub fn new(kind: TransportKind, peer: &str, caps: TransportCaps) -> Self {
        Self {
            kind,
            peer: peer.to_string(),
            caps,
            shell: RemoteShell::Posix,
        }
    }

    /// Override the remote command dialect (e.g. WinRM sessions speak
    /// PowerShell/cmd instead of POSIX `sh`).
    ///
    /// ```
    /// use kash::transport::{RemoteShell, TransportCaps, TransportInfo, TransportKind};
    ///
    /// let info = TransportInfo::new(TransportKind::Winrm, "Admin@h:5985 (winrm)", TransportCaps::no_pty())
    ///     .with_shell(RemoteShell::Powershell);
    /// assert_eq!(info.shell, RemoteShell::Powershell);
    /// ```
    pub fn with_shell(mut self, shell: RemoteShell) -> Self {
        self.shell = shell;
        self
    }
}

/// Command dialect of the remote shell.
///
/// The session loop frames every injected command with nonce markers
/// (`SH_CMD_START_…` / `SH_CMD_DONE_…:exit`) and probes identity with a
/// one-liner — both must match the remote dialect:
///
/// * [`RemoteShell::Posix`] — `sh`/`bash`/`zsh`: `$?`, `$(…)`.
/// * [`RemoteShell::Powershell`] — `$LASTEXITCODE`, `$(…)`, double quotes.
/// * [`RemoteShell::Cmd`] — `%ERRORLEVEL%` via the `call echo %^…%`
///   re-expansion trick (plain `%ERRORLEVEL%` would expand stale at parse
///   time), `&` separators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemoteShell {
    /// POSIX shells (`sh`, `bash`, `zsh`). Default.
    #[default]
    Posix,
    /// Windows PowerShell.
    Powershell,
    /// Windows `cmd.exe`.
    Cmd,
}

/// A remote-shell transport: dial (or accept), authenticate, and yield a
/// shell byte stream.
///
/// Implementors do all protocol work — auth, PTY allocation, shell
/// normalisation (e.g. SSH's `exec bash`, mirroring the reverse shell's
/// `pty.spawn` upgrade) — and return plain stream halves. Everything
/// downstream (session loop, agent IPC, transfers, modules) is
/// transport-agnostic and shared.
pub trait Transport: Send + Sync {
    /// Establish the transport. Returns the split shell stream plus the
    /// transport's identity/capabilities for the session loop.
    ///
    /// The explicit `Send` bound keeps connectors usable from multi-threaded
    /// async runtimes without relying on `async fn` auto bounds.
    fn connect(
        &self,
    ) -> impl std::future::Future<Output = anyhow::Result<(BoxReader, BoxWriter, TransportInfo)>> + Send;
}

// ---------------------------------------------------------------------------
// Daemon spawner (shared by every `*-d` subcommand)
// ---------------------------------------------------------------------------

/// Build the argv for a background headless worker: the subcommand, its
/// extra args, then `--session <id> --headless`.
///
/// Pure — unit tested and doctested — so every transport spawns background
/// workers identically.
///
/// ```
/// use kash::transport::daemon_argv;
///
/// let argv = daemon_argv("ssh", &["root@h".to_string(), "--port".to_string(), "22".to_string()], "monkey");
/// assert_eq!(argv, vec!["ssh", "root@h", "--port", "22", "--session", "monkey", "--headless"]);
/// ```
pub fn daemon_argv(subcommand: &str, extra: &[String], session_id: &str) -> Vec<String> {
    let mut argv = Vec::with_capacity(extra.len() + 4);
    argv.push(subcommand.to_string());
    argv.extend(extra.iter().cloned());
    argv.push("--session".to_string());
    argv.push(session_id.to_string());
    argv.push("--headless".to_string());
    argv
}

/// Print `banner`, spawn a detached headless worker running
/// `daemon_argv(subcommand, extra, session_id)`, and return immediately.
///
/// The child inherits stdio as `/dev/null` and its own session (`setsid`),
/// so closing the terminal doesn't kill the background session.
/// Reconnect with `kash attach <session-id>`.
#[cfg(unix)]
pub fn spawn_daemon(subcommand: &str, extra: &[String], session_id: &str, banner: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::process::CommandExt;

    use anyhow::Context;

    print!("{banner}");
    let _ = std::io::stdout().flush();

    let exe = std::env::current_exe().context("cannot resolve current executable path")?;
    let null = std::fs::File::open("/dev/null").context("cannot open /dev/null")?;

    let argv = daemon_argv(subcommand, extra, session_id);
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&argv)
        .stdin(null.try_clone()?)
        .stdout(null.try_clone()?)
        .stderr(null);

    // Detach from the controlling terminal so SIGHUP on terminal close
    // doesn't kill the background session.
    unsafe {
        cmd.pre_exec(|| {
            // Become a new session leader so closing the parent terminal does not
            // send SIGHUP to the daemon.  setsid(2) fails with EPERM only when the
            // process is already a process-group leader, meaning it is already
            // detached — treat that as success.
            let _ = libc::setsid();
            Ok(())
        });
    }

    cmd.spawn().context("failed to spawn daemon process")?;
    Ok(())
}

/// `--daemon (-d)` is Unix-only; every transport shares this error.
#[cfg(not(unix))]
pub fn spawn_daemon(
    _subcommand: &str,
    _extra: &[String],
    _session_id: &str,
    _banner: &str,
) -> anyhow::Result<()> {
    anyhow::bail!("--daemon (-d) is not supported on this platform (Unix only)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_labels() {
        assert_eq!(TransportKind::Reverse.as_str(), "reverse");
        assert_eq!(TransportKind::Ssh.as_str(), "ssh");
        assert_eq!(TransportKind::Winrm.as_str(), "winrm");
        assert_eq!(TransportKind::default(), TransportKind::Reverse);
    }

    #[test]
    fn daemon_argv_appends_session_and_headless() {
        let argv = daemon_argv("listen", &["4444".to_string()], "tiger");
        assert_eq!(argv, vec!["listen", "4444", "--session", "tiger", "--headless"]);
    }

    #[test]
    fn daemon_argv_empty_extra() {
        let argv = daemon_argv("ssh", &[], "panda");
        assert_eq!(argv, vec!["ssh", "--session", "panda", "--headless"]);
    }
}
