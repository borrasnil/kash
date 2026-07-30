//! Shared utility functions: base64 encoding/decoding, terminal helpers.
//!
//! Provides standalone [`base64_encode`] and [`base64_decode`] used
//! by the obfuscation backends and the file-transfer module, as well as
//! [`key_to_bytes`] and [`raw_normalize`] shared between the session loop
//! and the attach client.

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
}
