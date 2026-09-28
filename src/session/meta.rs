//! Meta-commands: special commands intercepted by the handler.
//!
//! These are typed at the prompt in handler mode (or raw PTY mode) and
//! handled locally by kash rather than being sent to the remote shell.

use std::io::{self, Write};
use std::time::Duration;

use crossterm::{
    cursor,
    queue,
    terminal::{Clear, ClearType},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use crate::cli::ShellType;
use crate::prompt;
use crate::transfer;



// ---------------------------------------------------------------------------
// Meta-action enum
// ---------------------------------------------------------------------------

pub(super) enum MetaAction {
    Help,
    Clear,
    Download { remote: String, local: String },
    Upload { local: String, remote: String },
    /// Switch to raw PTY passthrough (shell must already be PTY).
    Pty,
    /// Send PTY upgrade command then switch to raw passthrough.
    Upgrade,
    /// Release the local terminal; keep the TCP connection alive.
    Detach,
}

pub(super) fn parse_meta_raw(buf: &[u8]) -> Option<MetaAction> {
    let s = std::str::from_utf8(buf).ok()?;
    parse_meta(s)
}

pub(super) fn parse_meta(cmd: &str) -> Option<MetaAction> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    match parts.first().copied() {
        Some("help") => Some(MetaAction::Help),
        Some("clear" | "cls") => Some(MetaAction::Clear),
        Some("pty") => Some(MetaAction::Pty),
        Some("upgrade") => Some(MetaAction::Upgrade),
        Some("detach") => Some(MetaAction::Detach),
        Some("download") => {
            if parts.len() < 2 {
                return Some(MetaAction::Help);
            }
            let remote = parts[1].to_string();
            let local = parts
                .get(2)
                .map(|s| s.to_string())
                .unwrap_or_else(|| remote.rsplit('/').next().unwrap_or(&remote).to_string());
            Some(MetaAction::Download { remote, local })
        }
        Some("upload") => {
            if parts.len() < 2 {
                return Some(MetaAction::Help);
            }
            let local = parts[1].to_string();
            let remote = parts.get(2).map(|s| s.to_string()).unwrap_or_else(|| {
                // Default: upload to current directory keeping the local filename.
                std::path::Path::new(&local)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&local)
                    .to_string()
            });
            Some(MetaAction::Upload { local, remote })
        }
        _ => None,
    }
}

/// Execute a meta-command intercepted while in raw PTY mode.
/// Raw mode stays active before and after; caller handles stty resync.
pub(super) async fn execute_raw_meta(
    action: MetaAction,
    writer: &mut OwnedWriteHalf,
    reader: &mut OwnedReadHalf,
    stdout: &mut io::Stdout,
    terminal_active: &mut bool,
    session_id: &str,
    shell_type: ShellType,
) -> anyhow::Result<()> {
    // Drain any pending remote bytes (CTRL+U echo, stale PTY feedback) so they
    // don't corrupt the display after the meta-action completes.
    {
        let mut drain = vec![0u8; 4096];
        loop {
            match tokio::time::timeout(
                Duration::from_millis(150),
                reader.read(&mut drain),
            ).await {
                Ok(Ok(n)) if n > 0 => {}
                _ => break,
            }
        }
    }
    write!(stdout, "\r\n")?;
    stdout.flush()?;
    match action {
        MetaAction::Help => {
            write!(stdout, "{}", prompt::help_text().replace('\n', "\r\n"))?;
        }
        MetaAction::Clear => {
            queue!(stdout, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
        }
        MetaAction::Download { remote, local } => {
            match transfer::download(reader, writer, &remote, &local, stdout, shell_type).await {
                Ok(()) => {}
                Err(e) => write!(stdout, "{}\r\n", prompt::banner_error(&e.to_string()))?,
            }
        }
        MetaAction::Upload { local, remote } => {
            match transfer::upload(reader, writer, &local, &remote, stdout, shell_type).await {
                Ok(()) => {}
                Err(e) => write!(stdout, "{}\r\n", prompt::banner_error(&e.to_string()))?,
            }
        }
        MetaAction::Pty => {
            write!(stdout, "\x1b[2m  [already in raw PTY mode — CTRL+Q for handler mode]\x1b[0m\r\n")?;
        }
        MetaAction::Upgrade => {
            write!(stdout, "\x1b[2m  [sending PTY upgrade...]\x1b[0m\r\n")?;
            stdout.flush()?;
            let upgrade = concat!(
                "python3 -c 'import pty; pty.spawn(\"/bin/bash\")' 2>/dev/null || ",
                "python -c 'import pty; pty.spawn(\"/bin/bash\")' 2>/dev/null || ",
                "script -qc /bin/bash /dev/null 2>/dev/null"
            );
            writer.write_all(upgrade.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            tokio::time::sleep(Duration::from_millis(1000)).await;
            write!(stdout, "\x1b[2m  [PTY upgraded]\x1b[0m\r\n")?;
        }
        MetaAction::Detach => {
            write!(stdout, "\r\n{}\r\n", prompt::banner_session_detached(session_id))?;
            stdout.flush()?;
            *terminal_active = false;
            #[cfg(unix)]
            unsafe {
                libc::raise(libc::SIGTSTP);
            }
        }
    }
    stdout.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Small utilities
// ---------------------------------------------------------------------------

pub(super) fn last_line(s: &str) -> String {
    s.lines().last().unwrap_or("").to_string()
}

pub(super) fn strip_done_marker(s: &str, nonce: Option<&str>) -> String {
    let Some(nonce) = nonce else {
        return s.to_string();
    };
    let marker = format!("SH_CMD_DONE_{}:", nonce);
    if let Some(pos) = s.find(&marker) {
        let after = s[pos..].find('\n').map_or(s.len(), |p| pos + p + 1);
        format!("{}{}", &s[..pos], &s[after..])
    } else {
        s.to_string()
    }
}

pub(super) fn strip_start_marker(s: &str, nonce: Option<&str>) -> String {
    let Some(nonce) = nonce else {
        return s.to_string();
    };
    let marker = format!("SH_CMD_START_{nonce}");
    if let Some(pos) = s.find(&marker) {
        let after = s[pos..].find('\n').map_or(s.len(), |p| pos + p + 1);
        format!("{}{}", &s[..pos], &s[after..])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_meta_pty_and_upgrade() {
        assert!(matches!(parse_meta("pty"), Some(MetaAction::Pty)));
        assert!(matches!(parse_meta("upgrade"), Some(MetaAction::Upgrade)));
        assert!(matches!(parse_meta("detach"), Some(MetaAction::Detach)));
    }

    #[test]
    fn parse_meta_basic() {
        assert!(matches!(parse_meta("help"), Some(MetaAction::Help)));
        assert!(matches!(parse_meta("clear"), Some(MetaAction::Clear)));
        assert!(matches!(parse_meta("cls"), Some(MetaAction::Clear)));
        assert!(matches!(
            parse_meta("download /etc/passwd"),
            Some(MetaAction::Download { .. })
        ));
        assert!(matches!(
            parse_meta("upload ./foo /tmp/bar"),
            Some(MetaAction::Upload { .. })
        ));
        // Single-arg form: remote defaults to local filename.
        assert!(matches!(
            parse_meta("upload ./foo"),
            Some(MetaAction::Upload { remote, .. }) if remote == "foo"
        ));
        assert!(parse_meta("upload").map_or(false, |a| matches!(a, MetaAction::Help)));
    }

    #[test]
    fn parse_meta_regular_returns_none() {
        assert!(parse_meta("ls -la").is_none());
        assert!(parse_meta("exit").is_none());
        assert!(parse_meta("whoami").is_none());
    }

    #[test]
    fn strip_done_marker_removes_line() {
        let s = "output\nSH_CMD_DONE_abc:0\n";
        assert_eq!(strip_done_marker(s, Some("abc")), "output\n");
    }

    #[test]
    fn strip_done_marker_no_nonce_passthrough() {
        assert_eq!(strip_done_marker("output\n", None), "output\n");
    }

    #[test]
    fn strip_done_marker_no_match_passthrough() {
        assert_eq!(
            strip_done_marker("output\n", Some("xyz")),
            "output\n"
        );
    }

    #[test]
    fn last_line_extracts_prompt() {
        assert_eq!(last_line("out\nwww-data@box:~$ "), "www-data@box:~$ ");
        assert_eq!(last_line("$ "), "$ ");
        assert_eq!(last_line(""), "");
    }

    #[test]
    fn last_line_multiline_no_trailing_nl() {
        assert_eq!(last_line("a\nb\nroot@host:~# "), "root@host:~# ");
    }
}
