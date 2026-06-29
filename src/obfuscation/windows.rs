//! Windows / PowerShell obfuscation (placeholder).
//!
//! The current implementation passes commands through unchanged.
//! This module exists so the trait-based dispatch works for
//! `ShellType::Windows` without runtime errors.

use super::ObfuscationStrategy;

/// Pass-through strategy for Windows targets.
///
/// Will be replaced with real PowerShell obfuscation in a later
/// release.
pub struct WindowsStrategy;

impl ObfuscationStrategy for WindowsStrategy {
    fn obfuscate(&self, cmd: &str) -> String {
        cmd.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_passthrough() {
        let s = WindowsStrategy;
        assert_eq!(s.obfuscate("dir"), "dir");
        assert_eq!(s.obfuscate("whoami"), "whoami");
    }

    #[test]
    fn windows_empty() {
        assert_eq!(WindowsStrategy.obfuscate(""), "");
    }
}
