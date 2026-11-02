//! Minimal `~/.ssh/config` parser — the pieces the outbound SSH client needs
//! to behave like `ssh(1)` for zero-extra-flags usage.
//!
//! Supported keywords per stanza: `HostName`, `User`, `Port`, `IdentityFile`
//! (cumulative). Unknown keywords are ignored; `Match` blocks are skipped
//! until the next `Host`. Resolution follows ssh semantics across all
//! matching stanzas: first obtained value wins per keyword, identity files
//! accumulate in file order.

use std::path::{Path, PathBuf};

/// Per-host resolved config values for one target.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SshConfig {
    /// Target alias this config was parsed for, lowercased — pattern
    /// matching against later stanzas (`Host *`) needs the target.
    pub matched: String,
    /// `HostName` from the first matching stanza.
    pub host_name: Option<String>,
    /// `User` from the first matching stanza.
    pub user: Option<String>,
    /// `Port` from the first matching stanza.
    pub port: Option<u16>,
    /// `IdentityFile` entries from all matching stanzas, in file order.
    pub identity_files: Vec<String>,
}

/// `~/.ssh/config` path, or `None` without a home directory.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| PathBuf::from(h).join(".ssh").join("config"))
}

/// Load and parse an ssh_config file for `target`. Missing file →
/// [`SshConfig::default()`], matching ssh's behaviour of continuing without
/// config.
pub fn load(path: &Path, target: &str) -> anyhow::Result<SshConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse(&text, target)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SshConfig::for_target(target)),
        Err(e) => Err(anyhow::anyhow!("cannot read {}: {e}", path.display())),
    }
}

/// Pattern scope state while parsing. `None` = top-of-file or inside a
/// skipped `Match` block.
struct Scope {
    patterns: Vec<String>,
}

/// Parse ssh_config `text` for target `target` (the alias/command-line host,
/// lowercased for matching). Pure so the resolution rules are unit testable.
pub fn parse(text: &str, target: &str) -> SshConfig {
    let mut cfg = SshConfig::for_target(target);
    let mut scope: Option<Scope> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = split_key_value(line) else {
            continue;
        };
        let key = key.to_ascii_lowercase();

        match key.as_str() {
            // A new stanza starts: `Host pattern...` (comma lists per ssh).
            "host" => {
                let pats = value
                    .unwrap_or_default()
                    .split(',')
                    .map(|p| p.trim().to_ascii_lowercase())
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>();
                // `Host` with no patterns defensively applies to nothing.
                scope = if pats.is_empty() { None } else { Some(Scope { patterns: pats }) };
            }
            // `Match` blocks need conditional evaluation — skip until `Host`.
            "match" => scope = None,
            _ => {
                let (Some(value), Some(s)) = (value, &scope) else {
                    continue; // value outside any stanza, or inside skipped Match
                };
                if !s.patterns.iter().any(|p| host_pattern_matches(p, &cfg.matched)) {
                    continue;
                }
                apply_value(&mut cfg, &key, value);
            }
        }
    }
    cfg
}

/// Split `Key value`, `Key\tvalue` or `Key=value` (ssh accepts all three).
fn split_key_value(line: &str) -> Option<(&str, Option<&str>)> {
    let pair = match line.split_once('=') {
        // Only treat `=` as separator when directly attached to the first
        // word (`IdentityFile=~/x`); otherwise fall back to whitespace.
        Some((k, v)) if !k.is_empty() && !k.contains(' ') && !k.contains('\t') => (k, v),
        _ => line.split_once(char::is_whitespace)?,
    };
    let key = pair.0.trim();
    if key.is_empty() {
        return None;
    }
    let value = pair.1.trim();
    Some((key, if value.is_empty() { None } else { Some(value) }))
}

/// ssh(1) wildcard semantics: `*` (any run), `?` (one char), case-insensitive.
fn host_pattern_matches(pattern: &str, target: &str) -> bool {
    glob_match(pattern, target)
}

fn glob_match(pattern: &str, target: &str) -> bool {
    let p = pattern.as_bytes();
    let t = target.as_bytes();
    // Classic greedy `*` matcher.
    let mut pi = 0;
    let mut ti = 0;
    let mut star: Option<usize> = None;
    let mut star_t = 0;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi].eq_ignore_ascii_case(&t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            star_t = ti;
        } else if let Some(sp) = star {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Apply one stanza value. First-obtained wins, except identity files
/// (cumulative, like ssh's IdentityFile).
fn apply_value(cfg: &mut SshConfig, key: &str, value: &str) {
    match key {
        "hostname" => {
            if cfg.host_name.is_none() {
                cfg.host_name = Some(value.to_string());
            }
        }
        "user" => {
            if cfg.user.is_none() {
                cfg.user = Some(value.to_string());
            }
        }
        "port" => {
            if cfg.port.is_none() {
                if let Ok(p) = value.parse::<u16>() {
                    if p > 0 {
                        cfg.port = Some(p);
                    }
                }
            }
        }
        "identityfile" if !cfg.identity_files.iter().any(|f| f == value) => {
            cfg.identity_files.push(value.to_string());
        }
        _ => {}
    }
}

/// A matched target, filled in during parsing for pattern evaluation in
/// later stanzas (`Host *` after `Host kali` still needs the target).
impl SshConfig {
    /// Empty config with `matched` preset — the baseline [`parse`] builds on.
    pub fn for_target(target: &str) -> Self {
        Self {
            matched: target.to_ascii_lowercase(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str, target: &str) -> SshConfig {
        parse(text, target)
    }

    #[test]
    fn empty_and_missing() {
        assert_eq!(parse("", "kali"), SshConfig::for_target("kali"));
        assert_eq!(
            parse("# only a comment\n\n", "kali"),
            SshConfig::for_target("kali")
        );
    }

    #[test]
    fn basic_stanza() {
        let c = cfg("Host kali\n  HostName 10.0.0.5\n  User parallels\n  Port 2222\n", "kali");
        assert_eq!(c.host_name.as_deref(), Some("10.0.0.5"));
        assert_eq!(c.user.as_deref(), Some("parallels"));
        assert_eq!(c.port, Some(2222));
    }

    #[test]
    fn keys_case_and_eq_forms() {
        let c = cfg("HOST kali\n hostname=10.0.0.5\n\tUSER\tparallels\n", "kali");
        assert_eq!(c.host_name.as_deref(), Some("10.0.0.5"));
        assert_eq!(c.user.as_deref(), Some("parallels"));
    }

    #[test]
    fn comments_and_unknown_keys_ignored() {
        // NOTE: ssh only honours full-line comments — a trailing `#` makes
        // the rest of the line part of the value, like real ssh_config.
        let c = cfg("Host kali\n  #comment\n  ForwardX11Trusted yes\n  User parallels\n", "kali");
        assert_eq!(c.user.as_deref(), Some("parallels"));
    }

    #[test]
    fn first_value_wins_across_stanzas() {
        let text = "Host kali\n  User parallels\n  HostName 10.0.0.5\nHost *\n  User kam\n  HostName generic\n";
        let c = cfg(text, "kali");
        assert_eq!(c.user.as_deref(), Some("parallels"));
        assert_eq!(c.host_name.as_deref(), Some("10.0.0.5"));
        // Non-matching target falls to the catch-all.
        let c2 = cfg(text, "otherhost");
        assert_eq!(c2.user.as_deref(), Some("kam"));
        assert_eq!(c2.host_name.as_deref(), Some("generic"));
    }

    #[test]
    fn identity_files_accumulate() {
        let text = "Host kali\n  IdentityFile ~/.ssh/a\nHost *\n  IdentityFile ~/.ssh/b\n";
        let c = cfg(text, "kali");
        assert_eq!(c.identity_files, vec!["~/.ssh/a", "~/.ssh/b"]);
        let c2 = cfg(text, "other");
        assert_eq!(c2.identity_files, vec!["~/.ssh/b"]);
    }

    #[test]
    fn match_block_skipped() {
        let text = "Match all\n  User wrong\nHost kali\n  User right\n";
        let c = cfg(text, "kali");
        assert_eq!(c.user.as_deref(), Some("right"));
    }

    #[test]
    fn wildcard_and_question_patterns() {
        let c = cfg("Host *.lab\n  User parallels\n", "box1.lab");
        assert_eq!(c.user.as_deref(), Some("parallels"));
        let c2 = cfg("Host lab-?\n  User parallels\n", "lab-7");
        assert_eq!(c2.user.as_deref(), Some("parallels"));
        let c3 = cfg("Host lab-?\n  User parallels\n", "lab-77");
        assert_eq!(c3.user, None);
    }

    #[test]
    fn comma_pattern_list() {
        let c = cfg("Host kali,web*\n  User parallels\n", "web3");
        assert_eq!(c.user.as_deref(), Some("parallels"));
        let c2 = cfg("Host kali,web*\n  User parallels\n", "db1");
        assert_eq!(c2.user, None);
    }

    #[test]
    fn port_values() {
        // Invalid port values are skipped so a later valid one can apply.
        let c = cfg("Host kali\n  Port notanumber\n  Port 2222\n", "kali");
        assert_eq!(c.port, Some(2222));
        let c2 = cfg("Host kali\n  Port 99999\n  Port 2222\n", "kali");
        assert_eq!(c2.port, Some(2222));
        let c3 = cfg("Host kali\n  Port 2222\n", "kali");
        assert_eq!(c3.port, Some(2222));
        let c4 = cfg("Host kali\n  Port notanumber\n", "kali");
        assert_eq!(c4.port, None);
    }

    #[test]
    fn match_target() {
        let c = SshConfig::for_target("KaLi");
        assert_eq!(c.matched, "kali");
    }
}
