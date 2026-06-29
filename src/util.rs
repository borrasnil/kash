//! Shared utility functions: base64 encoding/decoding.
//!
//! Provides standalone [`base64_encode`] and [`base64_decode`] used
//! by the obfuscation backends and the file-transfer module.

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
pub fn base64_decode(encoded: &str) -> Option<Vec<u8>> {
    if encoded.is_empty() {
        return Some(Vec::new());
    }
    if encoded.len() % 4 != 0 && !encoded.ends_with('=') {
        return None;
    }
    for c in encoded.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' | '=' => continue,
            _ => return None,
        }
    }
    let chars: Vec<char> = encoded.chars().collect();
    let mut result = Vec::with_capacity(chars.len() / 4 * 3);

    let decode_char = |c: char| -> Option<u32> {
        match c {
            'A'..='Z' => Some((c as u32) - ('A' as u32)),
            'a'..='z' => Some((c as u32) - ('a' as u32) + 26),
            '0'..='9' => Some((c as u32) - ('0' as u32) + 52),
            '+' => Some(62),
            '/' => Some(63),
            '=' => Some(0),
            _ => None,
        }
    };

    let mut i = 0;
    while i + 4 <= chars.len() {
        let c0 = decode_char(chars[i])?;
        let c1 = decode_char(chars[i + 1])?;
        let c2 = decode_char(chars[i + 2])?;
        let c3 = decode_char(chars[i + 3])?;
        let triple = (c0 << 18) | (c1 << 12) | (c2 << 6) | c3;

        result.push(((triple >> 16) & 0xFF) as u8);
        if chars[i + 2] != '=' {
            result.push(((triple >> 8) & 0xFF) as u8);
        }
        if chars[i + 3] != '=' {
            result.push((triple & 0xFF) as u8);
        }
        i += 4;
    }

    Some(result)
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
