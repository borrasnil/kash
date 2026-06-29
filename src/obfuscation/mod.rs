//! Command obfuscation for reverse shell handlers.
//!
//! Defines [`ObfuscationStrategy`] — a trait implemented by all obfuscation
//! backends — and [`create_strategy`] to select one at runtime.
//!
//! # Example
//!
//! ```rust
//! use shell_handler::cli::{ObfuscationLevel, ShellType};
//! use shell_handler::obfuscation::create_strategy;
//!
//! let engine = create_strategy(ObfuscationLevel::Medium, ShellType::Linux);
//! let noisy = engine.obfuscate("whoami");
//! assert_ne!(noisy, "whoami");
//! assert!(!noisy.is_empty());
//! ```

mod linux;
mod windows;

pub use linux::{HeavyLinux, LightLinux, MediumLinux};
pub use windows::WindowsStrategy;

use crate::cli::{ObfuscationLevel, ShellType};

/// Turns a clean command string into a noisy, obfuscated one.
///
/// Implementations must be `Send + Sync` so they can be shared across
/// async task boundaries.
pub trait ObfuscationStrategy: Send + Sync {
    fn obfuscate(&self, cmd: &str) -> String;
}

/// Returns an obfuscation backend for the given level and shell type.
///
/// When `ShellType::Auto` is used the Linux backends are selected.
///
/// # Example
///
/// ```rust
/// use shell_handler::cli::{ObfuscationLevel, ShellType};
/// use shell_handler::obfuscation::create_strategy;
///
/// let s = create_strategy(ObfuscationLevel::Light, ShellType::Linux);
/// assert!(s.obfuscate("ls -la").contains("ls") || s.obfuscate("ls -la").contains('$'));
/// ```
pub fn create_strategy(level: ObfuscationLevel, shell: ShellType) -> Box<dyn ObfuscationStrategy> {
    match (shell, level) {
        (ShellType::Linux, ObfuscationLevel::Light) => Box::new(LightLinux),
        (ShellType::Linux, ObfuscationLevel::Medium) => Box::new(MediumLinux),
        (ShellType::Linux, ObfuscationLevel::Heavy) => Box::new(HeavyLinux),
        (ShellType::Windows, ObfuscationLevel::Light) => Box::new(WindowsStrategy),
        (ShellType::Windows, ObfuscationLevel::Medium) => Box::new(WindowsStrategy),
        (ShellType::Windows, ObfuscationLevel::Heavy) => Box::new(WindowsStrategy),
        (ShellType::Auto, level) => match level {
            ObfuscationLevel::Light => Box::new(LightLinux),
            ObfuscationLevel::Medium => Box::new(MediumLinux),
            ObfuscationLevel::Heavy => Box::new(HeavyLinux),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_creates_light_linux() {
        let s = create_strategy(ObfuscationLevel::Light, ShellType::Linux);
        let result = s.obfuscate("ls");
        assert!(!result.is_empty());
    }

    #[test]
    fn factory_creates_medium_linux() {
        let s = create_strategy(ObfuscationLevel::Medium, ShellType::Linux);
        let result = s.obfuscate("ls -la");
        assert!(!result.is_empty());
    }

    #[test]
    fn factory_creates_heavy_linux() {
        let s = create_strategy(ObfuscationLevel::Heavy, ShellType::Linux);
        let result = s.obfuscate("ls -la");
        assert!(!result.is_empty());
    }

    #[test]
    fn factory_auto_defaults_to_linux() {
        let s = create_strategy(ObfuscationLevel::Light, ShellType::Auto);
        let obfuscated = (0..20).any(|_| s.obfuscate("ls -la") != "ls -la");
        assert!(
            obfuscated,
            "light obfuscation should modify at least 1 of 20 attempts"
        );
    }

    #[test]
    fn factory_windows_passthrough() {
        let s = create_strategy(ObfuscationLevel::Light, ShellType::Windows);
        assert_eq!(s.obfuscate("dir"), "dir");
    }

    #[test]
    fn empty_command_returns_empty() {
        let s = create_strategy(ObfuscationLevel::Light, ShellType::Linux);
        assert_eq!(s.obfuscate(""), "");
    }

    #[test]
    fn strategies_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LightLinux>();
        assert_send_sync::<MediumLinux>();
        assert_send_sync::<HeavyLinux>();
    }
}
