//! Linux/bash obfuscation backends.
//!
//! Three levels are provided:
//!
//! | Level | Behaviour |
//! |---|---|
//! | [`LightLinux`] | `""` insertion, trailing junk comments. |
//! | [`MediumLinux`] | 6 variants: temp file, base64 decode, eval, printf hex, rev reverse, `$0 -c` |
//! | [`HeavyLinux`] | 6 variants: double/triple base64, `/dev/shm`, printf octal, `$0` here-string |

use rand::seq::SliceRandom;
use rand::Rng;

use super::ObfuscationStrategy;
use crate::util::base64_encode;

// ---------------------------------------------------------------------------
// LightLinux
// ---------------------------------------------------------------------------

/// Light bash obfuscation — valid bash, visually noisy.
///
/// Randomly applies `""` insertion inside words and trailing
/// junk comments. Empty quotes concatenate away at runtime,
/// so the command executes correctly.
pub struct LightLinux;

impl ObfuscationStrategy for LightLinux {
    fn obfuscate(&self, cmd: &str) -> String {
        if cmd.is_empty() {
            return String::new();
        }

        let mut rng = rand::thread_rng();
        let tokens: Vec<&str> = cmd.split_whitespace().collect();
        if tokens.is_empty() {
            return cmd.to_string();
        }

        let mut result = String::with_capacity(cmd.len() + 16);

        for (ti, token) in tokens.iter().enumerate() {
            if ti > 0 {
                result.push(' ');
            }
            if token.is_empty() {
                continue;
            }
            if rng.gen_bool(0.5) {
                inject_empty_quotes(token, &mut rng, &mut result);
            } else {
                result.push_str(token);
            }
        }

        if rng.gen_bool(0.25) {
            let junk_len = rng.gen_range(2..10);
            result.push_str(" #");
            for _ in 0..junk_len {
                result.push(rng.gen_range(b'a'..=b'z') as char);
            }
        }

        result
    }
}

fn inject_empty_quotes(token: &str, rng: &mut impl Rng, out: &mut String) {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 1 {
        out.push_str(token);
        return;
    }

    let max_inserts = (chars.len() / 2).max(1);
    let insert_count = rng.gen_range(1..=max_inserts);

    let mut positions: Vec<usize> = (1..chars.len())
        .filter(|&i| chars[i].is_alphanumeric())
        .collect();

    if positions.is_empty() {
        out.push_str(token);
        return;
    }

    positions.shuffle(rng);
    positions.truncate(insert_count);
    positions.sort_unstable();

    let mut pos_idx = 0;
    for (i, &c) in chars.iter().enumerate() {
        out.push(c);
        if pos_idx < positions.len() && positions[pos_idx] == i {
            out.push_str("\"\"");
            pos_idx += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// MediumLinux
// ---------------------------------------------------------------------------

/// Medium bash obfuscation — 6 real-world evasion techniques.
///
/// Cycles between temp-file execute, base64 decode, eval, printf hex,
/// rev reverse, and `$0 -c` shell-agnostic execution.
pub struct MediumLinux;

impl ObfuscationStrategy for MediumLinux {
    fn obfuscate(&self, cmd: &str) -> String {
        if cmd.is_empty() {
            return String::new();
        }

        let mut rng = rand::thread_rng();

        match rng.gen_range(0..6) {
            0 => {
                let suffix: String = (0..6).map(|_| rng.gen_range(b'a'..=b'z') as char).collect();
                let tmp = format!("/tmp/.{suffix}");
                format!("echo {} > {tmp} && . {tmp} && rm -f {tmp}", sh_escape(cmd))
            }
            1 => {
                let b64 = base64_encode(cmd.as_bytes());
                format!(
                    "echo '{b64}' | base64 -d 2>/dev/null | sh 2>/dev/null || \
                     echo '{b64}' | base64 -d 2>/dev/null | bash 2>/dev/null"
                )
            }
            2 => {
                format!("eval \"{}\"", sh_escape(cmd))
            }
            3 => {
                let hex: String = cmd.bytes().map(|b| format!("\\x{:02x}", b)).collect();
                format!("printf '{}' | sh", hex)
            }
            4 => {
                let rev: String = cmd.chars().rev().collect();
                let escaped = rev.replace('\'', "'\\''");
                format!("echo '{}' | rev 2>/dev/null | sh", escaped)
            }
            _ => {
                let b64 = base64_encode(cmd.as_bytes());
                format!("$0 -c \"$(echo '{}' | base64 -d 2>/dev/null)\"", b64)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HeavyLinux
// ---------------------------------------------------------------------------

/// Heavy bash obfuscation — layered, multi-stage evasion techniques.
///
/// Cycles between double/triple base64, `/dev/shm` with base64,
/// printf octal encoding, and `$0` here-string execution.
pub struct HeavyLinux;

impl ObfuscationStrategy for HeavyLinux {
    fn obfuscate(&self, cmd: &str) -> String {
        if cmd.is_empty() {
            return String::new();
        }

        let mut rng = rand::thread_rng();

        match rng.gen_range(0..6) {
            0 => {
                let b64 = base64_encode(cmd.as_bytes());
                let inner = format!("echo '{b64}' | base64 -d | sh");
                let outer_b64 = base64_encode(inner.as_bytes());
                format!("echo '{outer_b64}' | base64 -d | sh")
            }
            1 => {
                let suffix: String = (0..8).map(|_| rng.gen_range(b'a'..=b'z') as char).collect();
                let path = format!("/dev/shm/.{suffix}");
                let b64 = base64_encode(cmd.as_bytes());
                format!("echo '{b64}' > {path} && base64 -d {path} | sh && rm -f {path}")
            }
            2 => {
                let suffix: String = (0..8).map(|_| rng.gen_range(b'a'..=b'z') as char).collect();
                let path = format!("/dev/shm/.{suffix}");
                let b64 = base64_encode(cmd.as_bytes());
                format!("echo '{b64}' > {path} && base64 -d {path} | bash && rm -f {path}")
            }
            3 => {
                let b64 = base64_encode(cmd.as_bytes());
                let inner = format!("echo '{b64}' | base64 -d | sh");
                let mid_b64 = base64_encode(inner.as_bytes());
                let mid = format!("echo '{mid_b64}' | base64 -d | sh");
                let outer_b64 = base64_encode(mid.as_bytes());
                format!("echo '{outer_b64}' | base64 -d | sh")
            }
            4 => {
                let oct: String = cmd.bytes().map(|b| format!("\\{:03o}", b)).collect();
                format!("eval \"$(printf '{}')\"", oct)
            }
            _ => {
                let b64 = base64_encode(cmd.as_bytes());
                format!("$0 <<< \"$(echo '{}' | base64 -d)\"", b64)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sh_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '\'' => out.push_str("'\\''"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::base64_encode;

    #[test]
    fn light_obfuscates_simple_command() {
        let strategy = LightLinux;
        let result = strategy.obfuscate("ls");
        assert!(!result.is_empty());
    }

    #[test]
    fn light_obfuscates_complex_command() {
        let strategy = LightLinux;
        let result = strategy.obfuscate("ls -la /tmp");
        assert!(!result.is_empty());
    }

    #[test]
    fn light_makes_valid_bash_concatenation() {
        let strategy = LightLinux;
        for _ in 0..100 {
            let result = strategy.obfuscate("cd /mnt");
            assert!(
                !result.contains("${}"),
                "result contains invalid ${{}}: {result:?}"
            );
            assert!(
                result.contains("cd"),
                "result should contain cd: {result:?}"
            );
            assert!(
                result.contains('/'),
                "result should contain path separator: {result:?}"
            );
        }
    }

    #[test]
    fn light_empty_input() {
        assert_eq!(LightLinux.obfuscate(""), "");
    }

    #[test]
    fn medium_uses_temp_file_or_base64() {
        let strategy = MediumLinux;
        for _ in 0..30 {
            let result = strategy.obfuscate("whoami");
            assert!(!result.is_empty());
            assert!(
                result.contains("echo")
                    || result.contains("base64")
                    || result.contains("eval")
                    || result.contains(". ")
                    || result.contains("printf")
                    || result.contains("rev")
                    || result.contains("$0 -c"),
                "medium output unrecognized: {result:?}"
            );
        }
    }

    #[test]
    fn medium_empty_input() {
        assert_eq!(MediumLinux.obfuscate(""), "");
    }

    #[test]
    fn heavy_uses_double_encoding_or_dev_shm() {
        let strategy = HeavyLinux;
        for _ in 0..30 {
            let result = strategy.obfuscate("id");
            assert!(!result.is_empty());
            assert!(
                result.contains("base64")
                    || result.contains("/dev/shm")
                    || result.contains("printf")
                    || result.contains("<<<"),
                "heavy output unrecognized: {result:?}"
            );
        }
    }

    #[test]
    fn heavy_empty_input() {
        assert_eq!(HeavyLinux.obfuscate(""), "");
    }

    #[test]
    fn base64_roundtrip() {
        let input = b"ls -la /tmp";
        let encoded = base64_encode(input);
        let decoded = base64_decode(&encoded);
        assert_eq!(decoded, input);
    }

    #[test]
    fn base64_padding() {
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[test]
    fn base64_empty() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_all_bytes() {
        let input: Vec<u8> = (0..=255).collect();
        let encoded = base64_encode(&input);
        let decoded = base64_decode(&encoded);
        assert_eq!(decoded, input);
    }

    fn base64_decode(encoded: &str) -> Vec<u8> {
        let chars: Vec<char> = encoded.chars().collect();
        let mut result = Vec::with_capacity(chars.len() / 4 * 3);

        let decode_char = |c: char| -> u32 {
            match c {
                'A'..='Z' => (c as u32) - ('A' as u32),
                'a'..='z' => (c as u32) - ('a' as u32) + 26,
                '0'..='9' => (c as u32) - ('0' as u32) + 52,
                '+' => 62,
                '/' => 63,
                '=' => 0,
                _ => 0,
            }
        };

        let mut i = 0;
        while i + 4 <= chars.len() {
            let c0 = decode_char(chars[i]);
            let c1 = decode_char(chars[i + 1]);
            let c2 = decode_char(chars[i + 2]);
            let c3 = decode_char(chars[i + 3]);
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

        result
    }

    #[test]
    fn light_idempotent_no_crash() {
        let s = LightLinux;
        for cmd in [
            "",
            " ",
            "\t",
            "echo hi",
            "ls -la | grep foo",
            "cat /etc/passwd",
        ] {
            let _ = s.obfuscate(cmd);
        }
    }

    #[test]
    fn medium_idempotent_no_crash() {
        let s = MediumLinux;
        for cmd in ["", " ", "echo hi", "ls -la | grep foo"] {
            let _ = s.obfuscate(cmd);
        }
    }

    #[test]
    fn heavy_idempotent_no_crash() {
        let s = HeavyLinux;
        for cmd in ["", " ", "echo hi", "rm -rf /"] {
            let _ = s.obfuscate(cmd);
        }
    }
}
