//! Script modules: plain bash / PowerShell / Python files run on sessions.
//!
//! Discovery scans `$KASH_MODULES`, `./modules`, then `~/.kash/modules`
//! (first name wins). A module is any `.sh` / `.ps1` / `.py` file with an
//! optional `# kash-module: shell=… desc="…"` header. Body `{{VAR}}`
//! placeholders are filled from `--set`.
//!
//! Delivery is two-tier: small scripts go inline as a single-line
//! base64-pipe wrapper (verbatim through the session, never obfuscated);
//! larger ones upload to a remote temp file and run from there.

use std::collections::HashMap;

use crate::util::base64_encode;

/// Rendered scripts at or below this size (bytes) go inline; larger ones
/// take the upload path. Keeps the one-liner clear of remote PTY input
/// limits (~4 KB in canonical mode).
pub const INLINE_LIMIT: usize = 2048;

// ---------------------------------------------------------------------------
// Module model + discovery
// ---------------------------------------------------------------------------

/// Target shell a module runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleShell {
    Linux,
    Windows,
    Any,
}

impl ModuleShell {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Windows => "windows",
            Self::Any => "any",
        }
    }

    /// Parse a shell name (`linux`/`bash`/`sh`, `windows`/`powershell`,
    /// `any`). `None` for unknown names.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "linux" | "bash" | "sh" => Some(Self::Linux),
            "windows" | "powershell" | "ps" => Some(Self::Windows),
            "any" | "all" => Some(Self::Any),
            _ => None,
        }
    }

    /// Default from file extension.
    pub fn for_extension(ext: &str) -> Option<Self> {
        match ext.to_lowercase().as_str() {
            "sh" => Some(Self::Linux),
            "ps1" => Some(Self::Windows),
            "py" => Some(Self::Linux),
            _ => None,
        }
    }
}

/// One discovered module file.
#[derive(Debug, Clone)]
pub struct Module {
    /// File stem (`enum-users` for `enum-users.sh`).
    pub name: String,
    pub path: std::path::PathBuf,
    pub shell: ModuleShell,
    pub desc: String,
    /// Directory it was found in (for the listing).
    pub source: String,
}

/// Directories searched in precedence order (first name wins).
pub fn module_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(extra) = std::env::var_os("KASH_MODULES") {
        for part in std::env::split_paths(&extra) {
            dirs.push(part);
        }
    }
    dirs.push(std::path::PathBuf::from("./modules"));
    if let Some(home) = home_dir() {
        dirs.push(home.join(".kash").join("modules"));
    }
    dirs
}

fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// Scan all module dirs. Returns modules sorted by name.
pub fn discover() -> Vec<Module> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for dir in module_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default();
            if ModuleShell::for_extension(ext).is_none() {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            if name.is_empty() || !seen.insert(name.clone()) {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let (shell, desc) = parse_header(&content, ext);
            out.push(Module {
                name,
                path,
                shell,
                desc,
                source: dir.to_string_lossy().into_owned(),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Find a module by name.
pub fn find(name: &str) -> Option<Module> {
    discover().into_iter().find(|m| m.name == name)
}

/// Parse `# kash-module: shell=… desc="…"` from the first 10 lines.
/// Missing keys fall back to extension default (shell) and `""` (desc).
pub fn parse_header(content: &str, ext: &str) -> (ModuleShell, String) {
    let mut shell = ModuleShell::for_extension(ext).unwrap_or(ModuleShell::Any);
    let mut desc = String::new();
    for line in content.lines().take(10) {
        let Some(rest) = line
            .trim_start_matches(['#', '/', ';', ' '])
            .strip_prefix("kash-module:")
        else {
            continue;
        };
        for token in split_kv(rest) {
            let (k, v) = match token.split_once('=') {
                Some(pair) => pair,
                None => continue,
            };
            match k.trim().to_lowercase().as_str() {
                "shell" => {
                    if let Some(s) = ModuleShell::parse(unquote(v.trim())) {
                        shell = s;
                    }
                }
                "desc" | "description" => {
                    desc = unquote(v.trim()).to_string();
                }
                _ => {}
            }
        }
        break;
    }
    (shell, desc)
}

/// Split `k=v` tokens, honouring double/single quotes.
fn split_kv(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    for c in s.chars() {
        match (c, quote) {
            ('"' | '\'', None) => quote = Some(c),
            (q, Some(open)) if q == open => quote = None,
            (' ' | '\t', None) => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn unquote(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"')
            || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// Templating
// ---------------------------------------------------------------------------

/// `{{VAR}}` placeholder names in body order (deduped).
pub fn placeholders(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let start = j;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
            {
                j += 1;
            }
            let mut k = j;
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            if j > start && k + 1 < bytes.len() && bytes[k] == b'}' && bytes[k + 1] == b'}' {
                let name = &body[start..j];
                if !name.is_empty()
                    && (name.as_bytes()[0].is_ascii_alphabetic() || name.as_bytes()[0] == b'_')
                    && !out.iter().any(|n: &String| n == name)
                {
                    out.push(name.to_string());
                }
                i = k + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Fill `{{VAR}}` placeholders. Errors on missing names.
pub fn render(body: &str, vars: &HashMap<String, String>) -> anyhow::Result<String> {
    let mut missing = Vec::new();
    for name in placeholders(body) {
        if !vars.contains_key(&name) {
            missing.push(name);
        }
    }
    if !missing.is_empty() {
        anyhow::bail!("missing --set values for: {}", missing.join(", "));
    }
    let mut out = body.to_string();
    for (k, v) in vars {
        let plain = format!("{{{{{k}}}}}");
        out = out.replace(&plain, v);
        // Whitespace-padded variants (`{{ VAR }}`).
        let mut i = 0;
        while let Some(start) = out[i..].find("{{") {
            let abs_start = i + start;
            let rest = &out[abs_start + 2..];
            let mut j = 0;
            while j < rest.len() && rest.as_bytes()[j].is_ascii_whitespace() {
                j += 1;
            }
            if rest[j..].starts_with(k.as_str()) {
                let mut l = j + k.len();
                while l < rest.len() && rest.as_bytes()[l].is_ascii_whitespace() {
                    l += 1;
                }
                if rest[l..].starts_with("}}") {
                    let abs_end = abs_start + 2 + l + 2;
                    out.replace_range(abs_start..abs_end, v);
                    i = abs_start + v.len();
                    continue;
                }
            }
            i = abs_start + 2;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Delivery builders (pure — unit tested; the session runs them verbatim)
// ---------------------------------------------------------------------------

/// Base64 of the script bytes.
pub fn script_b64(script: &str) -> String {
    base64_encode(script.as_bytes())
}

/// Bash one-liner running a base64 script from stdin.
pub fn bash_inline(script: &str) -> String {
    format!("echo '{}' | base64 -d | bash -s", script_b64(script))
}

/// Python one-liner. Probes for `python3` first — a bare `||` fallback
/// would re-run the script under the second interpreter on failure.
pub fn python_inline(script: &str) -> String {
    format!(
        "if command -v python3 >/dev/null 2>&1; then P=\"python3\"; else P=\"python -\"; fi; \
         echo '{}' | base64 -d | $P",
        script_b64(script)
    )
}

/// PowerShell one-liner (`-EncodedCommand` needs UTF-16LE base64).
pub fn ps_inline(script: &str) -> String {
    let mut utf16 = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    format!(
        "powershell -NoProfile -NonInteractive -EncodedCommand {}",
        base64_encode(&utf16)
    )
}

/// Remote temp path for the upload tier. Linux uses `/tmp` (universal);
/// Windows uses a relative name (resolved against the session's working
/// directory — the client cannot expand remote `%TEMP%`).
pub fn remote_temp(shell: ModuleShell, nonce: &str) -> String {
    match shell {
        ModuleShell::Windows => format!(".kash-run-{nonce}.ps1"),
        _ => format!("/tmp/.kash-run-{nonce}"),
    }
}

/// Launcher running an uploaded script, then removing it while preserving
/// the script's exit code (no bare `exit` — that would kill the shell).
/// Caller single-quotes `remote`; paths from [`remote_temp`] are safe.
pub fn bash_file_launcher(remote_quoted: &str) -> String {
    format!("bash {remote_quoted}; _m=$?; rm -f {remote_quoted}; (exit $_m)")
}

/// PowerShell file launcher. Exit-code fidelity is best-effort here:
/// `powershell.exe` does not reliably propagate script codes, so this
/// reports PowerShell's own code. Documented limitation.
pub fn ps_file_launcher(remote: &str) -> String {
    format!("powershell -NoProfile -NonInteractive -File {remote}; Remove-Item -Force {remote}")
}

/// Single-quote a string for POSIX shells.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_parses_shell_and_desc() {
        let (shell, desc) = parse_header("# kash-module: shell=linux desc=\"enum users\"\n echo hi", "sh");
        assert_eq!(shell, ModuleShell::Linux);
        assert_eq!(desc, "enum users");
    }

    #[test]
    fn header_defaults_by_extension() {
        assert_eq!(parse_header("echo hi", "sh").0, ModuleShell::Linux);
        assert_eq!(parse_header("echo hi", "ps1").0, ModuleShell::Windows);
        assert_eq!(parse_header("print(1)", "py").0, ModuleShell::Linux);
        assert_eq!(parse_header("echo hi", "txt").0, ModuleShell::Any);
        assert_eq!(parse_header("echo hi", "sh").1, "");
    }

    #[test]
    fn header_unknown_shell_keeps_default() {
        assert_eq!(parse_header("# kash-module: shell=plan9", "sh").0, ModuleShell::Linux);
    }

    #[test]
    fn placeholders_found_in_order_deduped() {
        assert_eq!(
            placeholders("a {{FOO}} b {{ BAR }} c {{FOO}} {{9bad}} {{}}"),
            vec!["FOO".to_string(), "BAR".to_string()]
        );
    }

    #[test]
    fn render_fills_all_forms() {
        let mut vars = HashMap::new();
        vars.insert("H".to_string(), "10.0.0.1".to_string());
        let out = render("scan {{H}} and {{ H }} end", &vars).unwrap();
        assert_eq!(out, "scan 10.0.0.1 and 10.0.0.1 end");
    }

    #[test]
    fn render_errors_on_missing() {
        let err = render("{{A}} {{B}}", &HashMap::new()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains('A') && msg.contains('B'), "{msg}");
    }

    #[test]
    fn inline_wrappers_are_single_line() {
        for cmd in [bash_inline("echo 'hi'\necho $HOME"), python_inline("print('hi')")] {
            assert!(!cmd.contains('\n'), "{cmd}");
            assert!(!cmd.contains('\'') || cmd.matches('\'').count() == 2, "{cmd}");
        }
        let ps = ps_inline("Write-Output 'hi'");
        assert!(ps.starts_with("powershell -NoProfile -NonInteractive -EncodedCommand "));
        // UTF-16LE round-trips through our own decoder.
        let b64 = ps.rsplit(' ').next().unwrap();
        let raw = crate::util::base64_decode(b64).unwrap();
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), "Write-Output 'hi'");
    }

    #[test]
    fn bash_launcher_preserves_code_shape() {
        let l = bash_file_launcher("'/tmp/.kash-run-abc.sh'");
        assert!(l.starts_with("bash '/tmp/.kash-run-abc.sh'"));
        assert!(l.contains("(exit $_m)"));
    }

    #[test]
    fn remote_temp_paths() {
        assert!(remote_temp(ModuleShell::Linux, "n").starts_with("/tmp/.kash-run-n"));
        assert_eq!(remote_temp(ModuleShell::Windows, "n"), ".kash-run-n.ps1");
    }

    #[test]
    fn discover_finds_fixtures() {
        // Uses a temp KASH_MODULES dir so the real home is untouched.
        let dir = std::env::temp_dir().join(format!("kash-mod-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("enum.sh"), "# kash-module: shell=linux desc=\"enum\"\necho hi {{H}}\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "not a module").unwrap();
        // SAFETY: single-threaded test mutating our own env var.
        unsafe {
            std::env::set_var("KASH_MODULES", &dir);
        }
        let found = discover();
        let content = found
            .iter()
            .find(|m| m.name == "enum")
            .and_then(|m| std::fs::read_to_string(&m.path).ok())
            .unwrap_or_default();
        unsafe {
            std::env::remove_var("KASH_MODULES");
        }
        let _ = std::fs::remove_dir_all(&dir);
        let m = found.iter().find(|m| m.name == "enum").expect("enum found");
        assert_eq!(m.shell, ModuleShell::Linux);
        assert_eq!(m.desc, "enum");
        assert_eq!(placeholders(&content), vec!["H".to_string()]);
    }
}
