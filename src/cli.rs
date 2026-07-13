use clap::{Args as ClapArgs, Parser, Subcommand};

// ---------------------------------------------------------------------------
// Shared enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObfuscationLevel {
    None,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
}

// ---------------------------------------------------------------------------
// Clap wrappers (for FromStr / Display)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ObfuscationLevelArg(pub ObfuscationLevel);

impl std::str::FromStr for ObfuscationLevelArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "none" => Ok(Self(ObfuscationLevel::None)),
            "light" => Ok(Self(ObfuscationLevel::Light)),
            "medium" => Ok(Self(ObfuscationLevel::Medium)),
            "heavy" => Ok(Self(ObfuscationLevel::Heavy)),
            _ => Err(format!("invalid obfuscation level '{s}': expected none | light | medium | heavy")),
        }
    }
}

impl std::fmt::Display for ObfuscationLevelArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            ObfuscationLevel::None => write!(f, "none"),
            ObfuscationLevel::Light => write!(f, "light"),
            ObfuscationLevel::Medium => write!(f, "medium"),
            ObfuscationLevel::Heavy => write!(f, "heavy"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ShellTypeArg(pub ShellType);

impl std::str::FromStr for ShellTypeArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(Self(ShellType::Auto)),
            "linux" => Ok(Self(ShellType::Linux)),
            "windows" => Ok(Self(ShellType::Windows)),
            _ => Err(format!("invalid shell type '{s}': expected auto | linux | windows")),
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

#[derive(Debug, Clone)]
pub struct OutputFormatArg(pub OutputFormat);

impl std::str::FromStr for OutputFormatArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "text" => Ok(Self(OutputFormat::Text)),
            "json" => Ok(Self(OutputFormat::Json)),
            _ => Err(format!("invalid format '{s}': expected text | json")),
        }
    }
}

impl std::fmt::Display for OutputFormatArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            OutputFormat::Text => write!(f, "text"),
            OutputFormat::Json => write!(f, "json"),
        }
    }
}

// ---------------------------------------------------------------------------
// Top-level CLI
// ---------------------------------------------------------------------------

/// Obfuscated reverse-shell handler with Docker-style session management.
///
/// # Examples
///
/// ```text
/// shell-handler listen 4444
/// shell-handler listen 4444 -o heavy -s linux
/// shell-handler ps
/// shell-handler ps -q
/// shell-handler exec <session-id> whoami
/// shell-handler exec <session-id> --format json cat /etc/passwd
/// shell-handler inspect <session-id>
/// shell-handler kill <session-id>
/// ```
#[derive(Parser, Debug)]
#[command(
    name = "shell-handler",
    about = "Reverse shell handler with obfuscation and session management",
    version,
    subcommand_required = true,
    arg_required_else_help = true,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start a listener and wait for an incoming reverse shell.
    Listen(ListenArgs),

    /// List active shell sessions.
    Ps(PsArgs),

    /// Execute a command in a running session.
    Exec(ExecArgs),

    /// Show detailed information about a session.
    Inspect(InspectArgs),

    /// Terminate a running session gracefully.
    Kill(KillArgs),
}

// ---------------------------------------------------------------------------
// Subcommand args
// ---------------------------------------------------------------------------

#[derive(ClapArgs, Debug)]
pub struct ListenArgs {
    /// TCP port to listen on.
    pub port: u16,

    /// Bind address (default: all interfaces).
    #[arg(short = 'l', long, default_value = "0.0.0.0")]
    pub listen: String,

    /// Obfuscation level applied to every sent command: none | light | medium | heavy.
    #[arg(short = 'o', long, default_value = "none")]
    pub obfuscation: ObfuscationLevelArg,

    /// Target shell type: auto | linux | windows.
    #[arg(short = 's', long, default_value = "auto")]
    pub shell: ShellTypeArg,

    /// Override the random session ID with a custom value (useful for scripts).
    #[arg(long)]
    pub session: Option<String>,
}

impl ListenArgs {
    pub fn obfuscation_level(&self) -> ObfuscationLevel {
        self.obfuscation.0
    }
    pub fn shell_type(&self) -> ShellType {
        self.shell.0
    }
}

#[derive(ClapArgs, Debug)]
pub struct PsArgs {
    /// Print only session IDs, one per line (machine-readable).
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Output as JSON array.
    #[arg(long)]
    pub json: bool,
}

#[derive(ClapArgs, Debug)]
pub struct ExecArgs {
    /// Session ID to inject the command into.
    pub session: String,

    /// Exact command string sent to the remote shell — no token splitting.
    /// Use this for commands that contain quotes, pipes, or other shell syntax.
    /// When present, any trailing positional tokens are ignored.
    ///
    /// Example:
    ///   shell-handler exec <id> --cmd "python3 -c \"print('hello')\""
    #[arg(long, short = 'c')]
    pub cmd: Option<String>,

    /// Command tokens (joined with spaces). Use --cmd for complex commands.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,

    /// Output format: text (default) or json.
    #[arg(long, default_value = "text")]
    pub format: OutputFormatArg,
}

impl ExecArgs {
    /// Returns the command string to send to the remote shell.
    ///
    /// If `--cmd` is used AND trailing tokens are also present (happens when an
    /// LLM calls `exec session --cmd python3 -c "..."` without outer quoting),
    /// the two parts are joined so the full command is preserved.
    pub fn command_str(&self) -> String {
        match &self.cmd {
            Some(cmd) if !self.command.is_empty() => {
                format!("{} {}", cmd, self.command.join(" "))
            }
            Some(cmd) => cmd.clone(),
            None => self.command.join(" "),
        }
    }
    pub fn output_format(&self) -> OutputFormat {
        self.format.0
    }
}

#[derive(ClapArgs, Debug)]
pub struct InspectArgs {
    /// Session ID to inspect.
    pub session: String,
}

#[derive(ClapArgs, Debug)]
pub struct KillArgs {
    /// Session ID to terminate.
    pub session: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn verify_cli() {
        Cli::command().debug_assert();
    }

    #[test]
    fn listen_minimal() {
        let cli = Cli::try_parse_from(["sh", "listen", "4444"]).unwrap();
        let Command::Listen(a) = cli.command else { panic!() };
        assert_eq!(a.port, 4444);
        assert_eq!(a.listen, "0.0.0.0");
        assert_eq!(a.obfuscation_level(), ObfuscationLevel::None);
        assert_eq!(a.shell_type(), ShellType::Auto);
        assert!(a.session.is_none());
    }

    #[test]
    fn listen_all_options() {
        let cli = Cli::try_parse_from([
            "sh", "listen", "9001", "-l", "10.0.0.5", "-o", "light", "-s", "linux",
            "--session", "abcd1234",
        ])
        .unwrap();
        let Command::Listen(a) = cli.command else { panic!() };
        assert_eq!(a.port, 9001);
        assert_eq!(a.listen, "10.0.0.5");
        assert_eq!(a.obfuscation_level(), ObfuscationLevel::Light);
        assert_eq!(a.shell_type(), ShellType::Linux);
        assert_eq!(a.session.as_deref(), Some("abcd1234"));
    }

    #[test]
    fn ps_default() {
        let cli = Cli::try_parse_from(["sh", "ps"]).unwrap();
        let Command::Ps(a) = cli.command else { panic!() };
        assert!(!a.quiet);
        assert!(!a.json);
    }

    #[test]
    fn ps_quiet() {
        let cli = Cli::try_parse_from(["sh", "ps", "-q"]).unwrap();
        let Command::Ps(a) = cli.command else { panic!() };
        assert!(a.quiet);
    }

    #[test]
    fn ps_json() {
        let cli = Cli::try_parse_from(["sh", "ps", "--json"]).unwrap();
        let Command::Ps(a) = cli.command else { panic!() };
        assert!(a.json);
    }

    #[test]
    fn exec_single_word() {
        let cli = Cli::try_parse_from(["sh", "exec", "abc12345", "whoami"]).unwrap();
        let Command::Exec(a) = cli.command else { panic!() };
        assert_eq!(a.session, "abc12345");
        assert_eq!(a.command_str(), "whoami");
        assert_eq!(a.output_format(), OutputFormat::Text);
    }

    #[test]
    fn exec_multi_word() {
        let cli = Cli::try_parse_from(["sh", "exec", "abc12345", "ls", "-la", "/etc"]).unwrap();
        let Command::Exec(a) = cli.command else { panic!() };
        assert_eq!(a.command_str(), "ls -la /etc");
    }

    #[test]
    fn exec_json_format() {
        let cli =
            Cli::try_parse_from(["sh", "exec", "--format", "json", "abc12345", "whoami"]).unwrap();
        let Command::Exec(a) = cli.command else { panic!() };
        assert_eq!(a.output_format(), OutputFormat::Json);
        assert_eq!(a.command_str(), "whoami");
    }

    #[test]
    fn exec_cmd_flag_single_string() {
        let cli = Cli::try_parse_from([
            "sh", "exec", "abc12345", "--cmd", "python3 -c \"print('hello')\"",
        ])
        .unwrap();
        let Command::Exec(a) = cli.command else { panic!() };
        assert_eq!(a.session, "abc12345");
        assert_eq!(a.command_str(), "python3 -c \"print('hello')\"");
        assert!(a.cmd.is_some());
        assert!(a.command.is_empty());
    }

    #[test]
    fn exec_cmd_short_flag() {
        let cli = Cli::try_parse_from(["sh", "exec", "abc12345", "-c", "whoami"]).unwrap();
        let Command::Exec(a) = cli.command else { panic!() };
        assert_eq!(a.command_str(), "whoami");
    }

    #[test]
    fn exec_cmd_flag_with_format() {
        let cli = Cli::try_parse_from([
            "sh", "exec", "--format", "json", "abc12345", "--cmd", "id",
        ])
        .unwrap();
        let Command::Exec(a) = cli.command else { panic!() };
        assert_eq!(a.command_str(), "id");
        assert_eq!(a.output_format(), OutputFormat::Json);
    }

    #[test]
    fn exec_cmd_flag_takes_priority_over_positional() {
        // --cmd wins even if trailing tokens are also present
        let cli = Cli::try_parse_from([
            "sh", "exec", "abc12345", "--cmd", "echo hello",
        ])
        .unwrap();
        let Command::Exec(a) = cli.command else { panic!() };
        assert_eq!(a.command_str(), "echo hello");
    }

    #[test]
    fn inspect_parses() {
        let cli = Cli::try_parse_from(["sh", "inspect", "abc12345"]).unwrap();
        let Command::Inspect(a) = cli.command else { panic!() };
        assert_eq!(a.session, "abc12345");
    }

    #[test]
    fn kill_parses() {
        let cli = Cli::try_parse_from(["sh", "kill", "abc12345"]).unwrap();
        let Command::Kill(a) = cli.command else { panic!() };
        assert_eq!(a.session, "abc12345");
    }

    #[test]
    fn no_subcommand_fails() {
        assert!(Cli::try_parse_from(["sh"]).is_err());
    }

    #[test]
    fn invalid_obfuscation_level() {
        assert!(Cli::try_parse_from(["sh", "listen", "4444", "-o", "extreme"]).is_err());
    }

    #[test]
    fn invalid_shell_type() {
        assert!(Cli::try_parse_from(["sh", "listen", "4444", "-s", "macos"]).is_err());
    }

    #[test]
    fn listen_missing_port_fails() {
        assert!(Cli::try_parse_from(["sh", "listen"]).is_err());
    }
}
