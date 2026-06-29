//! Colored terminal output helpers.
//!
//! ANSI escape codes for banner messages, prompts, and status
//! indicators. All functions return [`String`]s that can be written
//! directly to stdout — they are NOT sent through the output
//! cleaner (which strips ANSI from remote shell data).

use std::net::SocketAddr;

/// Colored connection banner: `[+] Reverse shell connected from <addr>`
pub fn banner_connected(peer: &SocketAddr) -> String {
    format!("\x1b[1;32m[+]\x1b[0m Reverse shell connected from \x1b[1;33m{peer}\x1b[0m")
}

/// Colored disconnection notice: `[-] Connection closed`
pub fn banner_disconnected() -> String {
    "\x1b[1;31m[-]\x1b[0m Connection closed".to_string()
}

/// Colored file-saved notice: `[+] File saved to <path>`
pub fn banner_file_saved(path: &str) -> String {
    format!("\x1b[1;32m[+]\x1b[0m File saved to \x1b[1;33m{path}\x1b[0m")
}

/// Colored error notice: `[-] <msg>`
pub fn banner_error(msg: &str) -> String {
    format!("\x1b[1;31m[-]\x1b[0m {msg}")
}

/// Interactive prompt string.
pub fn prompt(peer: &SocketAddr) -> String {
    format!("\x1b[1;32mshell-handler\x1b[0m \x1b[1;36m({peer})\x1b[0m > ")
}

/// The `help` meta-command text.
pub fn help_text() -> &'static str {
    "\
\x1b[1;33mMeta-commands\x1b[0m (run inside the session):
  \x1b[1;32mhelp\x1b[0m                       Show this help
  \x1b[1;32mexit\x1b[0m / \x1b[1;32mquit\x1b[0m               Exit the session
  \x1b[1;32mclear\x1b[0m                     Clear the terminal
  \x1b[1;32mdownload <remote> [local]\x1b[0m  Download a file from the victim
  \x1b[1;32mupload <local> <remote>\x1b[0m     Upload a file to the victim

\x1b[1;33mObfuscation flags\x1b[0m:
  -o light | medium | heavy    (default: heavy)
  -s auto | linux | windows    (default: auto)
"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_contains_addr() {
        let addr: SocketAddr = "10.0.0.5:4444".parse().unwrap();
        let b = banner_connected(&addr);
        assert!(b.contains("10.0.0.5"));
        assert!(b.contains("4444"));
    }

    #[test]
    fn prompt_contains_addr() {
        let addr: SocketAddr = "10.0.0.5:4444".parse().unwrap();
        let p = prompt(&addr);
        assert!(p.contains("10.0.0.5"));
        assert!(p.contains("shell-handler"));
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
    fn help_text_is_comprehensive() {
        let h = help_text();
        assert!(h.contains("download"));
        assert!(h.contains("upload"));
        assert!(h.contains("help"));
        assert!(h.contains("exit"));
        assert!(h.contains("clear"));
    }
}
