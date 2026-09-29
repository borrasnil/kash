//! Update notification: warn when a newer kash release exists on GitHub.
//!
//! Checks `https://github.com/borrasnil/kash` releases (not crates.io — the
//! `kash` name is already taken there by an unrelated crate).
//!
//! * Fail-open — network errors, no releases yet, or rate limits never break
//!   the command being run.
//! * Cached — at most one network request per 24 h (see [`cache_path`]).
//! * `stderr`-only — `stdout` stays clean for `ps --json` / `exec --format json`.
//! * TTY-gated — only warns when `stderr` is a terminal, so scripts aren't spammed.
//! * Opt-out — `--no-update-check` flag or `KASH_NO_UPDATE_CHECK=1`.

use std::io::IsTerminal;
use std::time::Duration;

const CRATE_NAME: &str = "kash";
const RELEASES_API_URL: &str = "https://api.github.com/repos/borrasnil/kash/releases/latest";
const REPO_URL: &str = "https://github.com/borrasnil/kash";
/// Minimum seconds between network checks.
const CHECK_TTL_SECS: u64 = 24 * 3600;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Check for a newer release and print a one-line warning to `stderr`.
///
/// Call once at startup. Never fails — all errors are swallowed.
pub fn maybe_warn(no_update_check: bool) {
    if no_update_check || env_opt_out() {
        return;
    }
    // Don't spam scripts / piped output.
    if !std::io::stderr().is_terminal() {
        return;
    }

    let current = env!("CARGO_PKG_VERSION");

    // Fast path: fresh cache, no network.
    if let Some((cached, _)) = read_cache() {
        if is_newer(&cached, current) {
            print_warning(&cached, current);
        }
        return;
    }

    // Slow path: cache missing or stale — one short network request.
    match fetch_latest() {
        Some(latest) => {
            write_cache(&latest);
            if is_newer(&latest, current) {
                print_warning(&latest, current);
            }
        }
        None => {
            // Negative cache: don't hammer the network on every invocation
            // while offline or unpublished — retry in 24 h.
            write_cache(current);
        }
    }
}

fn env_opt_out() -> bool {
    matches!(
        std::env::var("KASH_NO_UPDATE_CHECK")
            .unwrap_or_default()
            .to_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

fn print_warning(latest: &str, current: &str) {
    // Fallible write, result ignored: a closed/broken stderr must never
    // panic the program (eprintln! would panic on write failure).
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr(),
        "\x1b[1;33m[!]\x1b[0m kash v{latest} available (you have v{current}) — \
         see {REPO_URL}/releases  \x1b[2m(to update: cargo install --force --git {REPO_URL})\x1b[0m"
    );
}

// ---------------------------------------------------------------------------
// Version comparison (no extra deps)
// ---------------------------------------------------------------------------

/// True if `latest` is strictly newer than `current`.
///
/// Parses leading `major.minor.patch` triplets numerically; non-numeric
/// suffixes (`-beta`, `+build`) are ignored. Anything unparsable → false
/// (fail-closed: never warn on garbage).
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_triplet(latest), parse_triplet(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn parse_triplet(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.strip_prefix('v').unwrap_or(v);
    let mut parts = v.split('.');
    let major = numeric_prefix(parts.next()?)?;
    let minor = numeric_prefix(parts.next()?)?;
    let patch = numeric_prefix(parts.next()?)?;
    Some((major, minor, patch))
}

/// Leading run of ASCII digits as u64 (`"2-beta"` → `Some(2)`, `"x"` → `None`).
fn numeric_prefix(s: &str) -> Option<u64> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

// ---------------------------------------------------------------------------
// Cache (cross-platform, plain text: "<version> <unix_ts>\n")
// ---------------------------------------------------------------------------

fn cache_path() -> Option<std::path::PathBuf> {
    let mut dir: std::path::PathBuf = if cfg!(windows) {
        // No CWD fallback: polluting the working dir with cache files is
        // worse than skipping the cache. temp_dir() always exists.
        std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    } else if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME")?;
        std::path::PathBuf::from(home).join("Library/Caches")
    } else {
        // Linux / other Unix: XDG_CACHE_HOME or ~/.cache
        if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
            std::path::PathBuf::from(xdg)
        } else {
            let home = std::env::var_os("HOME")?;
            std::path::PathBuf::from(home).join(".cache")
        }
    };
    dir.push("kash");
    dir.push("update-check");
    Some(dir)
}

/// Returns `(latest_version, checked_at)` if the cache exists and is fresh.
fn read_cache() -> Option<(String, u64)> {
    let path = cache_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    let mut it = content.split_whitespace();
    let version = it.next()?.to_string();
    let checked_at: u64 = it.next()?.parse().ok()?;
    let now = now_secs();
    if now.saturating_sub(checked_at) > CHECK_TTL_SECS {
        return None; // stale
    }
    if parse_triplet(&version).is_none() {
        return None;
    }
    Some((version, checked_at))
}

fn write_cache(version: &str) {
    let Some(path) = cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{version} {} \n", now_secs()));
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Network (blocking, short timeout — called at most once per 24 h)
// ---------------------------------------------------------------------------

/// Fetch `tag_name` (e.g. `"v0.3.0"`) from the GitHub Releases API.
/// `None` on any error (offline, no releases yet, rate-limited, bad JSON).
fn fetch_latest() -> Option<String> {
    let agent: ureq::Agent = ureq::AgentBuilder::new()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(&format!(
            "{CRATE_NAME}/{} ({REPO_URL})",
            env!("CARGO_PKG_VERSION")
        ))
        .build();
    // GitHub API requires an Accept header for versioned responses.
    let body = agent
        .get(RELEASES_API_URL)
        .set("Accept", "application/vnd.github+json")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    parse_tag_name(&body)
}

/// Extract `"tag_name":"vX.Y.Z"` from the GitHub release JSON without serde.
fn parse_tag_name(json: &str) -> Option<String> {
    let key = "\"tag_name\"";
    let pos = json.find(key)?;
    let rest = &json[pos + key.len()..];
    let colon = rest.find(':')?;
    let mut value = rest[colon + 1..].trim_start();
    // Strip optional surrounding quotes.
    if let Some(stripped) = value.strip_prefix('"') {
        value = stripped;
        let end = value.find('"')?;
        Some(value[..end].to_string())
    } else {
        let end = value
            .find(|c: char| c == ',' || c == '}' || c.is_whitespace())
            .unwrap_or(value.len());
        let v = value[..end].trim().to_string();
        if v.is_empty() { None } else { Some(v) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch() {
        assert!(is_newer("0.2.2", "0.2.1"));
        assert!(!is_newer("0.2.1", "0.2.1"));
        assert!(!is_newer("0.2.1", "0.2.2"));
    }

    #[test]
    fn newer_minor_major() {
        assert!(is_newer("0.3.0", "0.2.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.2.9", "0.3.0"));
    }

    #[test]
    fn multi_digit() {
        assert!(is_newer("0.2.10", "0.2.9"));
        assert!(!is_newer("0.2.9", "0.2.10"));
    }

    #[test]
    fn v_prefix_and_prerelease_ignored() {
        assert!(is_newer("v0.2.2", "0.2.1"));
        assert!(is_newer("0.2.2-beta", "0.2.1"));
        assert!(!is_newer("garbage", "0.2.1"));
        assert!(!is_newer("0.2.2", "garbage"));
    }

    #[test]
    fn parse_tag_name_basic() {
        let json = r#"{"tag_name":"v0.3.0","name":"v0.3.0"}"#;
        assert_eq!(parse_tag_name(json).as_deref(), Some("v0.3.0"));
    }

    #[test]
    fn parse_tag_name_with_spaces() {
        let json = r#"{ "tag_name" : "v1.2.3" }"#;
        assert_eq!(parse_tag_name(json).as_deref(), Some("v1.2.3"));
    }

    #[test]
    fn parse_tag_name_missing() {
        // No releases yet → 404 JSON has no tag_name.
        assert_eq!(parse_tag_name(r#"{"message":"Not Found"}"#), None);
    }
}
