use std::net::SocketAddr;

use crate::cli::{ObfuscationLevel, ShellType};

// ── ANSI palette ──────────────────────────────────────────────────────────────

const RST: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[1;31m";
const GREEN: &str = "\x1b[1;32m";
const YELLOW: &str = "\x1b[1;33m";
const CYAN: &str = "\x1b[1;36m";
const WHITE: &str = "\x1b[1;37m";

fn obf_color(level: ObfuscationLevel) -> &'static str {
    match level {
        ObfuscationLevel::None => "\x1b[2m",
        ObfuscationLevel::Light => "\x1b[1;32m",
        ObfuscationLevel::Medium => "\x1b[1;33m",
        ObfuscationLevel::Heavy => "\x1b[1;31m",
    }
}

pub fn obf_label(level: ObfuscationLevel) -> &'static str {
    match level {
        ObfuscationLevel::None => "none",
        ObfuscationLevel::Light => "light",
        ObfuscationLevel::Medium => "medium",
        ObfuscationLevel::Heavy => "heavy",
    }
}

pub fn shell_label(shell: ShellType) -> &'static str {
    match shell {
        ShellType::Auto => "auto",
        ShellType::Linux => "linux",
        ShellType::Windows => "windows",
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Startup banner printed once before the listener blocks (uses `\n`, pre-raw-mode).
pub fn startup_banner(
    listen: &str,
    port: u16,
    level: ObfuscationLevel,
    shell: ShellType,
    session_id: &str,
) -> String {
    let oc = obf_color(level);
    let ol = obf_label(level);
    let sl = shell_label(shell);
    let ver = env!("CARGO_PKG_VERSION");
    format!(
        "\n  {WHITE}shell-handler{RST}  {DIM}─────────────────────────────{RST}  {DIM}v{ver}{RST}\n\
         \n\
         {DIM}  obfuscation  :{RST}  {oc}{ol}{RST}\n\
         {DIM}  shell type   :{RST}  {CYAN}{sl}{RST}\n\
         {DIM}  listener     :{RST}  {BOLD}{listen}:{port}{RST}\n\
         {DIM}  session id   :{RST}  {CYAN}{session_id}{RST}\n\
         \n\
         {DIM}  [*] waiting for reverse connection...{RST}\n"
    )
}

/// `[+] shell connected from <addr>  •  session: <id>`
pub fn banner_connected(peer: &SocketAddr, session_id: &str) -> String {
    format!(
        "{GREEN}[+]{RST} shell connected from {WHITE}{peer}{RST}  {DIM}·  session:{RST} {CYAN}{session_id}{RST}"
    )
}

/// `[-] connection closed`
pub fn banner_disconnected() -> String {
    format!("{RED}[-]{RST} connection closed")
}

/// `[+] saved → <path>`
pub fn banner_file_saved(path: &str) -> String {
    format!("{GREEN}[+]{RST} saved {DIM}→{RST} {YELLOW}{path}{RST}")
}

/// `[-] <msg>`
pub fn banner_error(msg: &str) -> String {
    format!("{RED}[-]{RST} {msg}")
}

/// `[>] agent: <cmd>`  — shown in the TUI when an agent injects a command.
pub fn banner_agent_cmd(cmd: &str) -> String {
    format!("{CYAN}[>]{RST} {DIM}agent:{RST} {cmd}")
}

/// Printed when the user runs the `detach` meta-command.
/// Uses `\r\n` because it is printed while the terminal is still in raw mode.
pub fn banner_session_detached(session_id: &str) -> String {
    format!(
        "{YELLOW}[*]{RST} session {CYAN}{session_id}{RST} detached\r\n\
         {DIM}    ├ type 'bg' in your shell to resume in background{RST}\r\n\
         {DIM}    └ shell-handler attach {session_id}{RST} {DIM}to reconnect{RST}"
    )
}

/// Startup banner for daemon mode — printed before the parent exits.
pub fn startup_banner_daemon(
    listen: &str,
    port: u16,
    level: ObfuscationLevel,
    shell: ShellType,
    session_id: &str,
) -> String {
    let oc = obf_color(level);
    let ol = obf_label(level);
    let sl = shell_label(shell);
    let ver = env!("CARGO_PKG_VERSION");
    format!(
        "\n  {WHITE}shell-handler{RST}  {DIM}────────────────────────────{RST}  {DIM}v{ver}  daemon{RST}\n\
         \n\
         {DIM}  obfuscation  :{RST}  {oc}{ol}{RST}\n\
         {DIM}  shell type   :{RST}  {CYAN}{sl}{RST}\n\
         {DIM}  listener     :{RST}  {BOLD}{listen}:{port}{RST}\n\
         {DIM}  session id   :{RST}  {CYAN}{session_id}{RST}\n\
         \n\
         {DIM}  [*] listening in background — attach with:{RST}  shell-handler attach {session_id}\n"
    )
}

/// Printed in the attach client on successful connection.
pub fn banner_attach_connected(session_id: &str) -> String {
    format!(
        "{GREEN}[+]{RST} attached to session {CYAN}{session_id}{RST}  \
         {DIM}·  CTRL+Q to detach{RST}"
    )
}

/// Printed in the attach client when the user presses CTRL+Q to leave.
pub fn banner_attach_detached() -> String {
    format!("{YELLOW}[*]{RST} detached from session")
}

/// Printed in the attach client when the session closes the TCP connection.
pub fn banner_attach_session_closed() -> String {
    format!("{RED}[-]{RST} session closed")
}

/// Help text for the `help` meta-command (uses `\n`; caller converts to `\r\n` in raw mode).
pub fn help_text() -> &'static str {
    "\
\x1b[1;37mSession commands\x1b[0m \x1b[2m(from another terminal)\x1b[0m
  \x1b[1;32mshell-handler ps\x1b[0m                               list active sessions
  \x1b[1;32mshell-handler ps -q\x1b[0m                            list session IDs only
  \x1b[1;32mshell-handler exec\x1b[0m \x1b[2m<id> <command>\x1b[0m            run command in session
  \x1b[1;32mshell-handler exec --format json\x1b[0m \x1b[2m<id> <cmd>\x1b[0m  JSON output for LLMs
  \x1b[1;32mshell-handler upload\x1b[0m \x1b[2m<id> <local> [remote]\x1b[0m   push file via session IPC
  \x1b[1;32mshell-handler download\x1b[0m \x1b[2m<id> <remote> [local]\x1b[0m fetch file via session IPC
  \x1b[1;32mshell-handler inspect\x1b[0m \x1b[2m<id>\x1b[0m                   show session details
  \x1b[1;32mshell-handler kill\x1b[0m \x1b[2m<id>\x1b[0m                      terminate a session

\x1b[1;37mInteractive commands\x1b[0m \x1b[2m(type directly in raw PTY mode or handler mode)\x1b[0m
  \x1b[1;32mhelp\x1b[0m                         show this help
  \x1b[1;32mclear\x1b[0m                        clear screen
  \x1b[1;32mdownload\x1b[0m \x1b[2m<remote> [local]\x1b[0m    fetch file from target
  \x1b[1;32mupload\x1b[0m \x1b[2m<local> <remote>\x1b[0m      push file to target
  \x1b[1;32mdetach\x1b[0m                       release the terminal; TCP connection stays alive
                               type 'bg' to background, then shell-handler attach <id>
  \x1b[1;32mpty\x1b[0m                          \x1b[2m(handler mode)\x1b[0m switch to raw PTY passthrough
  \x1b[1;32mupgrade\x1b[0m                      \x1b[2m(handler mode)\x1b[0m re-send pty.spawn + raw PTY mode

\x1b[1;37mRaw PTY mode\x1b[0m \x1b[2m(default on connect for Linux/Auto targets)\x1b[0m
  All keystrokes forwarded verbatim — vim, python REPL, htop, ssh all work.
  \x1b[1;33mCTRL+Q\x1b[0m  switch to handler mode  \x1b[2m(any keyboard)\x1b[0m
  \x1b[1;33mCTRL+]\x1b[0m  switch to handler mode  \x1b[2m(US keyboard)\x1b[0m
  Terminal resize synced automatically via stty.

\x1b[1;37mHandler mode — signals\x1b[0m
  \x1b[1;33mCTRL+C\x1b[0m        send interrupt \x1b[2m(\\x03)\x1b[0m  — press twice to disconnect session
  \x1b[1;33mCTRL+Z\x1b[0m        send suspend  \x1b[2m(\\x1a)\x1b[0m to remote shell
  \x1b[1;33mCTRL+L\x1b[0m        clear screen
  \x1b[1;33mCTRL+D\x1b[0m        send EOF \x1b[2m(\\x04)\x1b[0m — exits python3 REPL, exits bash gracefully

\x1b[1;37mHandler mode — line editing\x1b[0m
  \x1b[2mCTRL+A / Home\x1b[0m   jump to start of line
  \x1b[2mCTRL+E / End\x1b[0m    jump to end of line
  \x1b[2mCTRL+U\x1b[0m          kill to start of line
  \x1b[2mCTRL+K\x1b[0m          kill to end of line
  \x1b[2mCTRL+W\x1b[0m          kill word backward
  \x1b[2m↑ / ↓\x1b[0m           history navigation
  \x1b[2mAlt+Enter\x1b[0m        insert newline for multiline commands

\x1b[1;37mObfuscation levels\x1b[0m
  \x1b[2m-o\x1b[0m  none \x1b[2m·\x1b[0m \x1b[1;32mlight\x1b[0m \x1b[2m·\x1b[0m \x1b[1;33mmedium\x1b[0m \x1b[2m·\x1b[0m \x1b[1;31mheavy\x1b[0m   \x1b[2m(default: none)\x1b[0m
  \x1b[2m-s\x1b[0m  auto \x1b[2m·\x1b[0m linux \x1b[2m·\x1b[0m windows              \x1b[2m(default: auto)\x1b[0m
"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_connected_contains_addr_and_session() {
        let addr: SocketAddr = "10.0.0.5:4444".parse().unwrap();
        let b = banner_connected(&addr, "abc12345");
        assert!(b.contains("10.0.0.5"));
        assert!(b.contains("4444"));
        assert!(b.contains("abc12345"));
    }

    #[test]
    fn banner_disconnected_not_empty() {
        assert!(!banner_disconnected().is_empty());
    }

    #[test]
    fn banner_error_contains_msg() {
        let e = banner_error("something broke");
        assert!(e.contains("something broke"));
    }

    #[test]
    fn banner_file_saved_contains_path() {
        let s = banner_file_saved("/tmp/loot.txt");
        assert!(s.contains("/tmp/loot.txt"));
    }

    #[test]
    fn banner_agent_cmd_contains_cmd() {
        let s = banner_agent_cmd("ls -la");
        assert!(s.contains("ls -la"));
        assert!(s.contains("agent"));
    }

    #[test]
    fn startup_banner_contains_session_id() {
        let s = startup_banner("0.0.0.0", 4444, ObfuscationLevel::Heavy, ShellType::Linux, "deadbeef");
        assert!(s.contains("shell-handler"));
        assert!(s.contains("deadbeef"));
        assert!(s.contains("4444"));
        assert!(s.contains("heavy"));
        assert!(s.contains("linux"));
    }

    #[test]
    fn help_text_is_comprehensive() {
        let h = help_text();
        assert!(h.contains("download"));
        assert!(h.contains("upload"));
        assert!(h.contains("help"));
        assert!(h.contains("exit"));
        assert!(h.contains("clear"));
        assert!(h.contains("CTRL+C"));
        assert!(h.contains("CTRL+L"));
        assert!(h.contains("exec"));
        assert!(h.contains("ps"));
        assert!(h.contains("kill"));
        assert!(h.contains("inspect"));
    }
}
