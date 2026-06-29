//! CLI argument parsing with type-safe wrappers.
//!
//! Uses [`clap`] derive for argument parsing. The newtype wrappers
//! [`ObfuscationLevelArg`] and [`ShellTypeArg`] implement [`FromStr`]
//! to provide validated, human-readable error messages for invalid
//! flag values.

use clap::Parser;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObfuscationLevel {
    Light,
    Medium,
    Heavy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellType {
    Auto,
    Linux,
    Windows,
}

#[derive(Debug, Clone)]
pub struct ObfuscationLevelArg(ObfuscationLevel);

impl std::str::FromStr for ObfuscationLevelArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "light" => Ok(Self(ObfuscationLevel::Light)),
            "medium" => Ok(Self(ObfuscationLevel::Medium)),
            "heavy" => Ok(Self(ObfuscationLevel::Heavy)),
            _ => Err(format!(
                "invalid obfuscation level '{s}': expected one of light, medium, heavy"
            )),
        }
    }
}

impl std::fmt::Display for ObfuscationLevelArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            ObfuscationLevel::Light => write!(f, "light"),
            ObfuscationLevel::Medium => write!(f, "medium"),
            ObfuscationLevel::Heavy => write!(f, "heavy"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ShellTypeArg(ShellType);

impl std::str::FromStr for ShellTypeArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(Self(ShellType::Auto)),
            "linux" => Ok(Self(ShellType::Linux)),
            "windows" => Ok(Self(ShellType::Windows)),
            _ => Err(format!(
                "invalid shell type '{s}': expected one of auto, linux, windows"
            )),
        }
    }
}

impl std::fmt::Display for ShellTypeArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            ShellType::Auto => write!(f, "auto"),
            ShellType::Linux => write!(f, "linux"),
            ShellType::Windows => write!(f, "windows"),
        }
    }
}

/// Listen for reverse shell connections and obfuscate commands.
///
/// Binds to a local address, waits for an inbound reverse shell,
/// then provides an interactive session where commands are
/// automatically obfuscated before being sent.
///
/// # Examples
///
/// ```ignore
/// // Listen on 0.0.0.0:4444 with light obfuscation (default):
/// //   shell-handler -p 4444
///
/// // Listen on a specific address with heavy obfuscation:
/// //   shell-handler -l 10.0.0.5 -p 4444 -o heavy
/// ```
#[derive(Parser, Debug)]
#[command(
    name = "shell-handler",
    about = "Reverse shell command obfuscation handler",
    version,
    long_about = "Listens for an inbound reverse shell, obfuscates every\ncommand before sending it, and displays clean output."
)]
pub struct Args {
    #[arg(
        short = 'l',
        long = "listen",
        default_value = "0.0.0.0",
        help = "Address to bind the listener to"
    )]
    pub listen: String,

    #[arg(
        short = 'p',
        long = "port",
        help = "Port to listen on for reverse shell connections"
    )]
    pub port: u16,

    #[arg(
        short = 'o',
        long = "obfuscation",
        default_value = "heavy",
        help = "Obfuscation level"
    )]
    pub obfuscation: ObfuscationLevelArg,

    #[arg(
        short = 's',
        long = "shell",
        default_value = "auto",
        help = "Target shell type"
    )]
    pub shell: ShellTypeArg,
}

impl Args {
    pub fn obfuscation_level(&self) -> ObfuscationLevel {
        self.obfuscation.0
    }

    pub fn shell_type(&self) -> ShellType {
        self.shell.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn verify_cli() {
        Args::command().debug_assert();
    }

    #[test]
    fn parse_minimal_args() {
        let args = Args::try_parse_from(["shell-handler", "-p", "4444"]).unwrap();
        assert_eq!(args.listen, "0.0.0.0");
        assert_eq!(args.port, 4444);
        assert_eq!(args.obfuscation_level(), ObfuscationLevel::Heavy);
        assert_eq!(args.shell_type(), ShellType::Auto);
    }

    #[test]
    fn parse_custom_listen_addr() {
        let args = Args::try_parse_from(["sh", "-l", "192.168.1.100", "-p", "9999"]).unwrap();
        assert_eq!(args.listen, "192.168.1.100");
        assert_eq!(args.port, 9999);
    }

    #[test]
    fn parse_all_args() {
        let args = Args::try_parse_from([
            "sh", "-l", "10.0.0.5", "-p", "8080", "-o", "heavy", "-s", "linux",
        ])
        .unwrap();
        assert_eq!(args.listen, "10.0.0.5");
        assert_eq!(args.port, 8080);
        assert_eq!(args.obfuscation_level(), ObfuscationLevel::Heavy);
        assert_eq!(args.shell_type(), ShellType::Linux);
    }

    #[test]
    fn parse_long_flags() {
        let args = Args::try_parse_from(["app", "--listen", "0.0.0.0", "--port", "4444"]).unwrap();
        assert_eq!(args.listen, "0.0.0.0");
        assert_eq!(args.port, 4444);
    }

    #[test]
    fn invalid_obfuscation_level() {
        let result = Args::try_parse_from(["app", "-p", "80", "-o", "extreme"]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_shell_type() {
        let result = Args::try_parse_from(["app", "-p", "80", "-s", "macos"]);
        assert!(result.is_err());
    }

    #[test]
    fn missing_required_args() {
        let result = Args::try_parse_from(["app"]);
        assert!(result.is_err());
    }
}
