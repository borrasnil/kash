//! Shared protocol helpers for file transfer.
//!
//! Contains the base64 streaming state machine, marker-based read functions,
//! shell quoting, SHA-256 verification, and formatting utilities used by
//! both the Linux and Windows transfer paths.

use std::io::{self, Write as _};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use crate::util::{base64_decode, find_slice};

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Flush progress every N decoded bytes.
pub(super) const PROGRESS_EVERY: u64 = 512 * 1024;

/// Drain pending bytes from `reader` for at most `for_duration` total.
///
/// Uses a single deadline so a chatty remote (PS1 prompts, banners) cannot
/// extend the drain indefinitely by continuously sending data within the window.
pub(super) async fn drain_reader<R: AsyncRead + Unpin>(reader: &mut R, for_duration: Duration) {
    let deadline = tokio::time::Instant::now() + for_duration;
    let mut buf = vec![0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, reader.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {}
            _ => break,
        }
    }
}

/// Double-quote a path for PowerShell, escaping backticks, double-quotes, and
/// dollar signs (prevents variable expansion inside the quoted string).
pub(super) fn ps_quote(path: &str) -> String {
    format!(
        "\"{}\"",
        path.replace('`', "``").replace('"', "`\"").replace('$', "`$")
    )
}

/// Single-quote escaping for POSIX shell paths.
pub(super) fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

/// 8-char lowercase hex nonce (used for protocol markers).
pub(super) fn gen_nonce() -> String {
    crate::util::generate_nonce()
}

pub(super) fn fmt_bytes(b: u64) -> String {
    if b < 1024 {
        format!("{b} B")
    } else if b < 1_048_576 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else {
        format!("{:.1} MB", b as f64 / 1_048_576.0)
    }
}

/// Compute SHA-256 of a local file in pure Rust via `spawn_blocking`.
///
/// Never blocks the async executor — the file I/O and hashing run on the
/// blocking thread pool, so large files don't stall the session loop.
pub(super) async fn sha256_local(path: &str) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)
            .map_err(|e| anyhow::anyhow!("cannot open {path} for sha256: {e}"))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 65536];
        loop {
            use std::io::Read;
            let n = file
                .read(&mut buf)
                .map_err(|e| anyhow::anyhow!("read error computing sha256: {e}"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await
    .map_err(|e| anyhow::anyhow!("sha256 task panicked: {e}"))?
}

/// Parse the 64-char hex hash from `sha256sum` / `shasum` output.
///
/// Requires the FIRST whitespace-delimited token to be EXACTLY 64 hex chars —
/// a sha256 hash can never be more or fewer. This prevents matching bash prompt
/// echo lines (e.g. `bash-3.2$`) that start with a hex digit but aren't hashes.
pub(super) fn parse_sha256_from_output(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| l.split_whitespace().next())
        .find(|tok| tok.len() == 64 && tok.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|s| s.to_string())
}

/// Drain accumulated base64 chars to the file.
///
/// When `final_flush = false`: only complete groups of 4 are decoded (safe
/// for mid-stream use, no partial group / padding issues).
/// When `final_flush = true`: decode everything including the terminal group.
///
/// Returns the number of decoded bytes written.
pub(super) async fn flush_b64(
    acc: &mut Vec<u8>,
    file: &mut tokio::fs::File,
    final_flush: bool,
) -> anyhow::Result<u64> {
    if acc.is_empty() {
        return Ok(0);
    }

    let decodable = if final_flush {
        acc.len()
    } else {
        (acc.len() / 4) * 4
    };

    if decodable == 0 {
        return Ok(0);
    }

    let s = std::str::from_utf8(&acc[..decodable])
        .map_err(|e| anyhow::anyhow!("non-UTF8 in base64 stream: {e}"))?;
    let decoded = base64_decode(s)
        .ok_or_else(|| anyhow::anyhow!("invalid base64 content from remote"))?;

    let written = decoded.len() as u64;
    file.write_all(&decoded).await?;

    let remainder = acc[decodable..].to_vec();
    *acc = remainder;

    Ok(written)
}

/// Read from `reader` until `marker` appears in the accumulated text.
pub(super) async fn read_until_marker<R: AsyncRead + Unpin>(
    reader: &mut R,
    marker: &str,
) -> anyhow::Result<String> {
    read_until_marker_with_prefix(reader, marker, String::new()).await
}

/// Like `read_until_marker` but with already-accumulated prefix text.
///
/// Searches for `marker` in the raw byte buffer before converting to UTF-8,
/// so non-UTF-8 bytes from the remote (binary filenames, etc.) cannot corrupt
/// the ASCII-only marker via `from_utf8_lossy` replacement characters.
pub(super) async fn read_until_marker_with_prefix<R: AsyncRead + Unpin>(
    reader: &mut R,
    marker: &str,
    prefix: String,
) -> anyhow::Result<String> {
    const MAX_ACCUMULATE: usize = 16 * 1024 * 1024; // 16 MB guard
    let marker_bytes = marker.as_bytes();
    // Accumulate as raw bytes; convert to String only when done.
    let mut raw: Vec<u8> = prefix.into_bytes();
    if find_slice(&raw, marker_bytes).is_some() {
        return Ok(String::from_utf8_lossy(&raw).into_owned());
    }
    let mut buf = vec![0u8; 4096];
    loop {
        if raw.len() > MAX_ACCUMULATE {
            anyhow::bail!(
                "response too large (> 16 MB) waiting for '{marker}'; \
                 check that the remote command is producing output"
            );
        }
        let n = timeout(IDLE_TIMEOUT, reader.read(&mut buf))
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for '{marker}'"))?
            .map_err(|e| anyhow::anyhow!("read error: {e}"))?;
        if n == 0 {
            anyhow::bail!("connection closed waiting for '{marker}'");
        }
        raw.extend_from_slice(&buf[..n]);
        if find_slice(&raw, marker_bytes).is_some() {
            return Ok(String::from_utf8_lossy(&raw).into_owned());
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state machine for incoming base64 data
// ---------------------------------------------------------------------------

/// Receive a delimited base64 stream from `reader` and write decoded bytes to
/// `local_file`.  Shared by both the Linux and Windows download paths.
///
/// Returns the number of decoded bytes written.
pub(super) async fn recv_b64_to_file<R: AsyncRead + Unpin>(
    reader: &mut R,
    local_file: &mut tokio::fs::File,
    ds: &[u8],
    de: &[u8],
    dnf: &[u8],
    file_size: Option<u64>,
    remote_path: &str,
    local_path: &str,
    stdout: &mut io::Stdout,
) -> anyhow::Result<u64> {
    let max_delim_len = ds.len().max(de.len()).max(dnf.len());
    let mut carry: Vec<u8> = Vec::new();
    let mut carry_pos: usize = 0;
    const COMPACT_AT: usize = 1 << 20;

    let mut b64_acc: Vec<u8> = Vec::with_capacity(4096);
    let mut bytes_written: u64 = 0;
    let mut last_progress_at: u64 = 0;
    let mut state: u8 = 0;
    let mut raw_buf = vec![0u8; 65536];

    loop {
        let n = timeout(IDLE_TIMEOUT, reader.read(&mut raw_buf))
            .await
            .map_err(|_| anyhow::anyhow!("download timed out (no data for 30 s)"))?
            .map_err(|e| anyhow::anyhow!("read error: {e}"))?;

        if n == 0 {
            anyhow::bail!("connection closed during download");
        }

        carry.extend_from_slice(&raw_buf[..n]);

        if carry_pos > COMPACT_AT {
            carry.drain(..carry_pos);
            carry_pos = 0;
        }

        'process: loop {
            let buf = &carry[carry_pos..];

            match state {
                0 => {
                    let nf_pos = find_slice(buf, dnf);
                    let st_pos = find_slice(buf, ds);

                    let notfound = match (nf_pos, st_pos) {
                        (Some(_), None) => true,
                        (Some(nf), Some(st)) if nf < st => true,
                        _ => false,
                    };
                    if notfound {
                        anyhow::bail!("remote file not found: {remote_path}");
                    }

                    match st_pos {
                        Some(pos) => {
                            carry_pos += pos + ds.len();
                            state = 1;
                        }
                        None => {
                            if buf.len() >= max_delim_len {
                                carry_pos = carry.len() - (max_delim_len - 1);
                            }
                            break 'process;
                        }
                    }
                }
                1 => {
                    let buf = &carry[carry_pos..];
                    match find_slice(buf, de) {
                        Some(pos) => {
                            for &b in &buf[..pos] {
                                if !b.is_ascii_whitespace() {
                                    b64_acc.push(b);
                                }
                            }
                            bytes_written += flush_b64(&mut b64_acc, local_file, true).await?;
                            state = 2;
                            break 'process;
                        }
                        None => {
                            let safe = buf.len().saturating_sub(de.len() - 1);
                            for &b in &buf[..safe] {
                                if !b.is_ascii_whitespace() {
                                    b64_acc.push(b);
                                }
                            }
                            bytes_written += flush_b64(&mut b64_acc, local_file, false).await?;
                            carry_pos += safe;

                            if bytes_written.saturating_sub(last_progress_at) >= PROGRESS_EVERY {
                                last_progress_at = bytes_written;
                                if let Some(fs) = file_size {
                                    let pct = if fs > 0 { bytes_written * 100 / fs } else { 100 };
                                    write!(
                                        stdout,
                                        "  \x1b[2m[↓]\x1b[0m  {} → {}   {} / {}  \x1b[2m{}%\x1b[0m\r",
                                        remote_path, local_path,
                                        fmt_bytes(bytes_written), fmt_bytes(fs), pct
                                    )?;
                                } else {
                                    write!(
                                        stdout,
                                        "  \x1b[2m[↓]\x1b[0m  {} → {}   {}\r",
                                        remote_path, local_path, fmt_bytes(bytes_written)
                                    )?;
                                }
                                stdout.flush()?;
                            }
                            break 'process;
                        }
                    }
                }
                _ => break 'process,
            }
        }

        if state >= 2 {
            break;
        }
    }

    local_file.flush().await?;
    Ok(bytes_written)
}

pub(super) fn print_transfer_result(
    stdout: &mut io::Stdout,
    arrow: &str,
    from_path: &str,
    to_path: &str,
    bytes: u64,
    local_hash: Option<&str>,
    remote_hash: Option<&str>,
) -> io::Result<()> {
    match (local_hash, remote_hash) {
        (Some(lh), Some(rh)) if lh == rh => {
            write!(
                stdout,
                "  \x1b[1;32m[✓]\x1b[0m  \x1b[2m[{arrow}]\x1b[0m  {} → {}   {}  \x1b[2m·  sha256 ok\x1b[0m\r\n",
                from_path, to_path, fmt_bytes(bytes)
            )?;
        }
        (Some(lh), Some(rh)) => {
            write!(
                stdout,
                "  \x1b[1;31m[✗]\x1b[0m  \x1b[2m[{arrow}]\x1b[0m  {} → {}   {}  \x1b[1;31msha256 MISMATCH\x1b[0m\r\n\
                 \x1b[2m       local:  {lh}\x1b[0m\r\n\
                 \x1b[2m       remote: {rh}\x1b[0m\r\n",
                from_path, to_path, fmt_bytes(bytes)
            )?;
        }
        _ => {
            write!(
                stdout,
                "  \x1b[1;33m[~]\x1b[0m  \x1b[2m[{arrow}]\x1b[0m  {} → {}   {}  \x1b[2m·  (sha256 unavailable on target)\x1b[0m\r\n",
                from_path, to_path, fmt_bytes(bytes)
            )?;
        }
    }
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_basic() {
        assert_eq!(shell_quote("/etc/passwd"), "'/etc/passwd'");
    }

    #[test]
    fn shell_quote_single_quote_in_path() {
        assert_eq!(shell_quote("/tmp/it's"), "'/tmp/it'\\''s'");
    }

    #[test]
    fn fmt_bytes_units() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1024), "1.0 KB");
        assert_eq!(fmt_bytes(1_048_576), "1.0 MB");
    }

    #[test]
    fn parse_sha256_extracts_hash() {
        let output = "a3f8b1c2d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1  /etc/passwd\n";
        let h = parse_sha256_from_output(output).unwrap();
        assert_eq!(h, "a3f8b1c2d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1");
    }

    #[test]
    fn parse_sha256_returns_none_on_garbage() {
        assert!(parse_sha256_from_output("command not found\n").is_none());
    }

    #[test]
    fn parse_sha256_rejects_bash_prompt_echo() {
        // The old implementation matched this line because 'b' is a hex digit and the
        // line is longer than 64 chars. The new one requires exactly 64 hex chars.
        let echo = "bash-3.2$ sha256sum 'test.txt' 2>/dev/null; printf 'SHSHA_%s:done\\n' 'abc12345'";
        assert!(parse_sha256_from_output(echo).is_none());
    }

    #[test]
    fn parse_sha256_rejects_short_hex_token() {
        assert!(parse_sha256_from_output("deadbeef  /tmp/x\n").is_none());
    }

    #[test]
    fn parse_sha256_rejects_63_char_token() {
        let short = format!("{}  /tmp/x\n", "a".repeat(63));
        assert!(parse_sha256_from_output(&short).is_none());
    }
}
