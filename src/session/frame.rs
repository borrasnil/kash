//! Shell-dialect framing for agent-injected commands.
//!
//! Every `exec`/`run` payload is wrapped with nonce markers so the session
//! loop can extract output and exit code from the shared shell stream:
//!
//! * `SH_CMD_START_<nonce>` — everything before it is noise (PTY echo of
//!   the injected bytes, shell prompts) and is skipped.
//! * `SH_CMD_DONE_<nonce>:<exit>` — everything before it (after START) is
//!   the command's output; the trailing integer is its exit code.
//!
//! The wrapper syntax must match the remote dialect ([`RemoteShell`]):
//! POSIX `$?` means nothing to PowerShell (`$LASTEXITCODE`) or `cmd.exe`
//! (`%ERRORLEVEL%`, which additionally needs the `call echo %^…%`
//! re-expansion trick — a plain `%ERRORLEVEL%` expands stale at parse
//! time). All builders here are pure string functions with doctests.

use crate::transport::RemoteShell;

/// Wrap `payload` with START/DONE markers for `nonce`.
///
/// * `raw` — the session is in raw PTY passthrough: the PTY echoes the
///   injected bytes, so POSIX framing also wraps the payload in
///   `stty -echo` / `stty echo` to keep the echo (and the response) clean.
/// * `has_pty` — the transport has a PTY at all. PTY-less transports
///   (WinRM) still need the START marker because shell prompts precede
///   command output.
///
/// Behaviour for existing transports is unchanged: POSIX+raw keeps the
/// exact historical `stty` dance, POSIX+handler keeps the plain `; echo`
/// form.
///
/// ```
/// use kash::session::frame::agent_command;
/// use kash::transport::RemoteShell;
///
/// // POSIX raw PTY: echo suppression + both markers.
/// let c = agent_command(RemoteShell::Posix, "id", "abc12345", true, true);
/// assert!(c.contains("stty -echo"));
/// assert!(c.contains("echo 'SH_CMD_START_abc12345'"));
/// assert!(c.contains("echo 'SH_CMD_DONE_abc12345:'$_shec"));
///
/// // POSIX handler mode: plain form, no START marker (historical).
/// let c = agent_command(RemoteShell::Posix, "id", "abc12345", false, true);
/// assert!(!c.contains("SH_CMD_START"));
/// assert!(c.contains("echo 'SH_CMD_DONE_abc12345:'$?"));
///
/// // PowerShell: always START-framed, `$LASTEXITCODE` for the code.
/// let c = agent_command(RemoteShell::Powershell, "Get-Date", "n", false, false);
/// assert!(c.contains("echo \"SH_CMD_START_n\""));
/// assert!(c.contains("echo \"SH_CMD_DONE_n:$LASTEXITCODE\""));
///
/// // cmd.exe: `&` separators + re-expanded `%ERRORLEVEL%`.
/// let c = agent_command(RemoteShell::Cmd, "dir", "n", false, false);
/// assert!(c.contains("echo SH_CMD_START_n"));
/// assert!(c.contains("call echo SH_CMD_DONE_n:%^ERRORLEVEL%"));
/// ```
pub fn agent_command(
    shell: RemoteShell,
    payload: &str,
    nonce: &str,
    raw: bool,
    has_pty: bool,
) -> String {
    match shell {
        RemoteShell::Posix if raw => format!(
            "stty -echo 2>/dev/null; echo 'SH_CMD_START_{nonce}'; \
             {payload}; _shec=$?; \
             stty echo 2>/dev/null; echo 'SH_CMD_DONE_{nonce}:'$_shec"
        ),
        RemoteShell::Posix if !has_pty => format!(
            "echo 'SH_CMD_START_{nonce}'; {payload}; echo 'SH_CMD_DONE_{nonce}:'$?"
        ),
        RemoteShell::Posix => format!("{payload}; echo 'SH_CMD_DONE_{nonce}:'$?"),
        RemoteShell::Powershell => format!(
            "echo \"SH_CMD_START_{nonce}\"; {payload}; echo \"SH_CMD_DONE_{nonce}:$LASTEXITCODE\""
        ),
        RemoteShell::Cmd => format!(
            "echo SH_CMD_START_{nonce} & {payload} & call echo SH_CMD_DONE_{nonce}:%^ERRORLEVEL%"
        ),
    }
}

/// Whether command output must be extracted by skipping everything before
/// the START marker: raw PTY echo, non-POSIX prompts, or PTY-less transports
/// whose shell prints prompts before output.
///
/// ```
/// use kash::session::frame::framed_output;
/// use kash::transport::RemoteShell;
///
/// assert!(framed_output(true, true, RemoteShell::Posix));   // raw PTY echo
/// assert!(!framed_output(false, true, RemoteShell::Posix)); // handler mode (historical)
/// assert!(framed_output(false, false, RemoteShell::Posix)); // PTY-less: prompt noise
/// assert!(framed_output(false, false, RemoteShell::Powershell));
/// assert!(framed_output(true, true, RemoteShell::Cmd));
/// ```
pub fn framed_output(raw: bool, has_pty: bool, shell: RemoteShell) -> bool {
    raw || shell != RemoteShell::Posix || !has_pty
}

/// One-liner printing `SHIDENTITY:<user>:<host>` in the remote dialect.
/// The session loop parses the first matching line; everything else the
/// shell prints around it is ignored.
///
/// ```
/// use kash::session::frame::probe_command;
/// use kash::transport::RemoteShell;
///
/// assert!(probe_command(RemoteShell::Posix).contains("SHIDENTITY"));
/// assert!(probe_command(RemoteShell::Powershell).contains("$env:COMPUTERNAME"));
/// assert!(probe_command(RemoteShell::Cmd).contains("%USERNAME%"));
/// ```
pub fn probe_command(shell: RemoteShell) -> &'static str {
    match shell {
        RemoteShell::Posix => {
            "printf 'SHIDENTITY:%s:%s\\n' \"$(whoami)\" \"$(hostname -s)\"\n"
        }
        RemoteShell::Powershell => "\"SHIDENTITY:$(whoami):$env:COMPUTERNAME\"\n",
        RemoteShell::Cmd => "echo SHIDENTITY:%USERNAME%:%COMPUTERNAME%\n",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_raw_keeps_historical_shape() {
        let c = agent_command(RemoteShell::Posix, "ls", "n", true, true);
        assert!(c.starts_with("stty -echo"));
        assert!(c.contains("_shec=$?"));
    }

    #[test]
    fn payload_passes_through_verbatim() {
        let payload = "cat /etc/passwd | grep root";
        for shell in [RemoteShell::Posix, RemoteShell::Powershell, RemoteShell::Cmd] {
            assert!(agent_command(shell, payload, "n", false, false).contains(payload));
        }
    }
}
