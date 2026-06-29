//! ANSI/control-character cleaner for reverse-shell output.
//!
//! Removes CSI sequences (`\x1b[...letter`), OSC sequences
//! (`\x1b]...\x07` / `\x1b]...\x1b\`), bare escapes, carriage
//! returns, null bytes, and bell characters. Preserves `\n`, `\t`,
//! printable ASCII, and UTF-8 multi-byte sequences.

/// Strip ANSI escape sequences and control characters from shell output.
///
/// Removes CSI sequences (`\x1b[...letter`), OSC sequences
/// (`\x1b]...\x07` or `\x1b]...\x1b\`), bare escapes, carriage
/// returns, null bytes, and bell characters. Preserves `\n`, `\t`,
/// printable ASCII, and UTF-8 multi-byte sequences.
///
/// # Examples
///
/// ```
/// use shell_handler::output::clean_output;
///
/// let clean = clean_output(b"\x1b[31mred\x1b[0m normal");
/// assert_eq!(clean, "red normal");
///
/// // carriage returns are stripped
/// let clean = clean_output(b"line1\r\nline2\r\n");
/// assert_eq!(clean, "line1\nline2\n");
/// ```
pub fn clean_output(data: &[u8]) -> String {
    let mut cleaned = Vec::with_capacity(data.len());
    let mut i = 0;

    while i < data.len() {
        match data[i] {
            0x1b => {
                i += 1;
                if i >= data.len() {
                    break;
                }
                match data[i] {
                    b'[' => {
                        i += 1;
                        while i < data.len() {
                            let b = data[i];
                            if b.is_ascii_alphabetic() || b == b'~' {
                                i += 1;
                                break;
                            }
                            i += 1;
                        }
                    }
                    b']' => {
                        i += 1;
                        while i < data.len() {
                            if data[i] == 0x07 {
                                i += 1;
                                break;
                            }
                            if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            0x0d => {
                i += 1;
            }
            0x07 | 0x00 => {
                i += 1;
            }
            b => {
                cleaned.push(b);
                i += 1;
            }
        }
    }

    String::from_utf8_lossy(&cleaned).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(clean_output(b"hello world"), "hello world");
    }

    #[test]
    fn strips_carriage_returns() {
        assert_eq!(clean_output(b"line1\r\nline2\r\n"), "line1\nline2\n");
    }

    #[test]
    fn strips_simple_ansi() {
        let input = b"\x1b[31mred\x1b[0m";
        assert_eq!(clean_output(input), "red");
    }

    #[test]
    fn strips_cursor_move() {
        let input = b"line1\x1b[Alinemoved";
        assert_eq!(clean_output(input), "line1linemoved");
    }

    #[test]
    fn strips_clear_screen() {
        let input = b"before\x1b[2Jafter";
        assert_eq!(clean_output(input), "beforeafter");
    }

    #[test]
    fn strips_bell_and_null() {
        let input = b"a\x07b\x00c";
        assert_eq!(clean_output(input), "abc");
    }

    #[test]
    fn strips_osc_sequence_slash() {
        let input = b"text\x1b]0;title\x1b\\more";
        assert_eq!(clean_output(input), "textmore");
    }

    #[test]
    fn strips_osc_sequence_bell() {
        let input = b"text\x1b]0;title\x07more";
        assert_eq!(clean_output(input), "textmore");
    }

    #[test]
    fn empty_input() {
        assert_eq!(clean_output(b""), "");
    }

    #[test]
    fn only_ansi() {
        assert_eq!(clean_output(b"\x1b[1;32m"), "");
    }

    #[test]
    fn utf8_preserved() {
        let input = "héllo wörld".as_bytes();
        assert_eq!(clean_output(input), "héllo wörld");
    }

    #[test]
    fn complex_ansi() {
        let input = b"\x1b[38;2;255;100;0mcolored\x1b[0m normal";
        assert_eq!(clean_output(input), "colored normal");
    }

    #[test]
    fn incomplete_ansi_at_end() {
        let input = b"text\x1b[3";
        assert_eq!(clean_output(input), "text");
    }

    #[test]
    fn multiple_newlines() {
        let input = b"line1\nline2\n\nline4";
        assert_eq!(clean_output(input), "line1\nline2\n\nline4");
    }

    #[test]
    fn tabs_preserved() {
        let input = b"col1\tcol2\tcol3";
        assert_eq!(clean_output(input), "col1\tcol2\tcol3");
    }

    #[test]
    fn random_bytes_no_panic() {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        for _ in 0..1000 {
            let len = rng.gen_range(0..256);
            let bytes: Vec<u8> = (0..len).map(|_| rng.gen_range(0..=255)).collect();
            let _ = clean_output(&bytes);
        }
    }
}
