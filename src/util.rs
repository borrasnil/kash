//! Shared utility functions: base64 encoding/decoding, terminal helpers,
//! time formatting, JSON escaping, and nonce generation.
//!
//! Provides standalone functions used across multiple modules:
//!
//! * [`base64_encode`] / [`base64_decode`] — used by obfuscation backends
//!   and the file-transfer module.
//! * [`key_to_bytes`] / [`raw_normalize`] — shared between the session loop
//!   and the attach client.
//! * [`json_str`] — JSON string escaping for `ps --json` and `exec --format json`.
//! * [`time_ago`] / [`unix_to_datetime_str`] / [`days_to_ymd`] — human-readable
//!   timestamps for `inspect`.
//! * [`generate_nonce`] — random hex nonces for protocol markers.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Base64-encode an arbitrary byte slice.
pub fn base64_encode(bytes: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;

    while i < bytes.len() {
        let b0 = bytes[i] as u32;
        let b1 = bytes.get(i + 1).copied().unwrap_or(0) as u32;
        let b2 = bytes.get(i + 2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);

        if i + 1 < bytes.len() {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        if i + 2 < bytes.len() {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        i += 3;
    }

    result
}

/// Base64-decode a string. Returns `None` on invalid input.
///
/// Single-pass: validation and decoding are interleaved via a lookup table,
/// so the input is traversed exactly once.
pub fn base64_decode(encoded: &str) -> Option<Vec<u8>> {
    // -1 = invalid, 64 = padding ('='), 0-63 = value
    const TABLE: [i8; 256] = {
        let mut t = [-1i8; 256];
        let mut i = 0u8;
        while i < 26 { t[(b'A' + i) as usize] = i as i8; i += 1; }
        i = 0;
        while i < 26 { t[(b'a' + i) as usize] = (26 + i) as i8; i += 1; }
        i = 0;
        while i < 10 { t[(b'0' + i) as usize] = (52 + i) as i8; i += 1; }
        t[b'+' as usize] = 62;
        t[b'/' as usize] = 63;
        t[b'=' as usize] = 64; // padding sentinel
        t
    };

    if encoded.is_empty() {
        return Some(Vec::new());
    }
    let bytes = encoded.as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }

    let mut result = Vec::with_capacity(bytes.len() / 4 * 3);

    for chunk in bytes.chunks(4) {
        let v0 = TABLE[chunk[0] as usize];
        let v1 = TABLE[chunk[1] as usize];
        let v2 = TABLE[chunk[2] as usize];
        let v3 = TABLE[chunk[3] as usize];

        // v < 0 means invalid character; 64 means '=' padding
        if v0 < 0 || v1 < 0 || v2 < 0 || v3 < 0 {
            return None;
        }
        // Padding may only appear in the last two positions
        if v0 == 64 || v1 == 64 {
            return None;
        }

        let triple = ((v0 as u32) << 18)
            | ((v1 as u32) << 12)
            | (((v2 & 63) as u32) << 6)
            | ((v3 & 63) as u32);

        result.push(((triple >> 16) & 0xFF) as u8);
        if v2 != 64 { result.push(((triple >> 8) & 0xFF) as u8); }
        if v3 != 64 { result.push((triple & 0xFF) as u8); }
    }

    Some(result)
}

/// Convert a crossterm `KeyEvent` to the byte sequence a terminal emulator would send.
pub fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) if ctrl => vec![(c as u8) & 0x1f],
        KeyCode::Char(c) if alt => {
            let mut v = vec![0x1b];
            v.extend_from_slice(c.encode_utf8(&mut [0u8; 4]).as_bytes());
            v
        }
        KeyCode::Char(c) => c.encode_utf8(&mut [0u8; 4]).as_bytes().to_vec(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::F(1) => vec![0x1b, b'O', b'P'],
        KeyCode::F(2) => vec![0x1b, b'O', b'Q'],
        KeyCode::F(3) => vec![0x1b, b'O', b'R'],
        KeyCode::F(4) => vec![0x1b, b'O', b'S'],
        KeyCode::F(5) => vec![0x1b, b'[', b'1', b'5', b'~'],
        KeyCode::F(6) => vec![0x1b, b'[', b'1', b'7', b'~'],
        KeyCode::F(7) => vec![0x1b, b'[', b'1', b'8', b'~'],
        KeyCode::F(8) => vec![0x1b, b'[', b'1', b'9', b'~'],
        KeyCode::F(9) => vec![0x1b, b'[', b'2', b'0', b'~'],
        KeyCode::F(10) => vec![0x1b, b'[', b'2', b'1', b'~'],
        KeyCode::F(11) => vec![0x1b, b'[', b'2', b'3', b'~'],
        KeyCode::F(12) => vec![0x1b, b'[', b'2', b'4', b'~'],
        _ => vec![],
    }
}

/// Normalize bytes for raw-terminal output: bare `\n` → `\r\n`, strip null bytes.
pub fn raw_normalize(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    for &b in data {
        match b {
            0x00 => {}
            b'\n' => {
                if out.last() != Some(&b'\r') {
                    out.push(b'\r');
                }
                out.push(b'\n');
            }
            b => out.push(b),
        }
    }
    out
}

/// Find a sub-slice in a byte slice, returning the start index.
pub fn find_slice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Produce a JSON-quoted string with minimal escaping.
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Human-readable elapsed time: "just now", "5m ago", "2h 3m ago", "3d ago".
pub fn time_ago(ts: u64, now: u64) -> String {
    let elapsed = now.saturating_sub(ts);
    if elapsed < 60 {
        "just now".to_string()
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86400 {
        let h = elapsed / 3600;
        let m = (elapsed % 3600) / 60;
        if m > 0 { format!("{h}h {m}m ago") } else { format!("{h}h ago") }
    } else {
        let d = elapsed / 86400;
        let h = (elapsed % 86400) / 3600;
        if h > 0 { format!("{d}d {h}h ago") } else { format!("{d}d ago") }
    }
}

/// Format a Unix epoch as "YYYY-MM-DD HH:MM:SS UTC".
pub fn unix_to_datetime_str(secs: u64) -> String {
    if secs == 0 {
        return "unknown".to_string();
    }
    let days = secs / 86400;
    let rem = secs % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02} UTC")
}

/// Gregorian calendar date from days since 1970-01-01.
/// Algorithm: Howard Hinnant's civil_from_days.
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days as i64 + 719_468;
    let era: i64 = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + if mo <= 2 { 1 } else { 0 };
    (y as u64, mo, d)
}

/// 8-char lowercase hex nonce (used for protocol markers).
pub fn generate_nonce() -> String {
    use rand::Rng;
    format!("{:08x}", rand::thread_rng().r#gen::<u32>())
}

/// 16-char lowercase hex nonce (used for remote temp file names to resist races).
pub fn gen_temp_nonce() -> String {
    use rand::Rng;
    format!("{:016x}", rand::thread_rng().r#gen::<u64>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let data = b"hello world";
        assert_eq!(base64_decode(&base64_encode(data)).unwrap(), data);
    }

    #[test]
    fn encode_decode_all_bytes() {
        let data: Vec<u8> = (0..=255).collect();
        assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data);
    }

    #[test]
    fn encode_empty() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_decode("").unwrap(), vec![]);
    }

    #[test]
    fn encode_padding() {
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[test]
    fn decode_invalid_returns_none() {
        assert!(base64_decode("!!!").is_none());
    }

    #[test]
    fn find_slice_works() {
        assert_eq!(find_slice(b"hello world", b"world"), Some(6));
        assert_eq!(find_slice(b"abc", b"x"), None);
        assert_eq!(find_slice(b"abc", b""), Some(0));
        assert_eq!(find_slice(b"aaa", b"aa"), Some(0));
    }

    #[test]
    fn json_str_escapes_special_chars() {
        let s = json_str("hello\nworld\"test\\path");
        assert_eq!(s, r#""hello\nworld\"test\\path""#);
    }

    #[test]
    fn json_str_plain_string() {
        assert_eq!(json_str("whoami"), r#""whoami""#);
    }

    #[test]
    fn json_str_empty() {
        assert_eq!(json_str(""), r#""""#);
    }

    #[test]
    fn unix_to_datetime_str_epoch() {
        // 1970-01-01 00:00:00 UTC
        assert_eq!(unix_to_datetime_str(0), "unknown");
        assert_eq!(unix_to_datetime_str(86400), "1970-01-02 00:00:00 UTC");
    }

    #[test]
    fn unix_to_datetime_str_known_date() {
        // 2024-01-01 00:00:00 UTC → 19723 days after epoch
        let secs = 19723 * 86400u64;
        assert_eq!(unix_to_datetime_str(secs), "2024-01-01 00:00:00 UTC");
    }

    #[test]
    fn days_to_ymd_known_values() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
        assert_eq!(days_to_ymd(365), (1971, 1, 1));
        assert_eq!(days_to_ymd(19723), (2024, 1, 1));
    }

    #[test]
    fn time_ago_just_now() {
        assert_eq!(time_ago(100, 120), "just now");
    }

    #[test]
    fn time_ago_minutes() {
        assert_eq!(time_ago(0, 300), "5m ago");
    }

    #[test]
    fn time_ago_hours() {
        assert_eq!(time_ago(0, 3661), "1h 1m ago");
        assert_eq!(time_ago(0, 7200), "2h ago");
    }

    #[test]
    fn time_ago_days() {
        assert_eq!(time_ago(0, 90000), "1d 1h ago");
        assert_eq!(time_ago(0, 172800), "2d ago");
    }

    #[test]
    fn nonce_is_8_hex_chars() {
        let n = generate_nonce();
        assert_eq!(n.len(), 8);
        assert!(n.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn raw_normalize_adds_cr() {
        assert_eq!(raw_normalize(b"a\nb"), b"a\r\nb");
    }

    #[test]
    fn raw_normalize_no_double_cr() {
        assert_eq!(raw_normalize(b"a\r\nb"), b"a\r\nb");
    }

    #[test]
    fn raw_normalize_strips_null() {
        assert_eq!(raw_normalize(b"a\x00b"), b"ab");
    }

    #[test]
    fn key_to_bytes_ctrl_a() {
        let key = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), vec![0x01]);
    }

    #[test]
    fn key_to_bytes_arrow_up() {
        let key = KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), vec![0x1b, b'[', b'A']);
    }

    #[test]
    fn key_to_bytes_f1() {
        let key = KeyEvent {
            code: KeyCode::F(1),
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), vec![0x1b, b'O', b'P']);
    }
}
