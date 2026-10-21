//! CLI argument parsing using `clap`.
//!
//! Defines the top-level CLI structure, subcommand arguments, and shared
//! enums. The actual command handlers live in [`commands`].

pub mod commands;

use clap::{Args as ClapArgs, Parser, Subcommand};

// ---------------------------------------------------------------------------
// Shared enums
// ---------------------------------------------------------------------------

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

/// Reverse-shell handler with Docker-style session management.
///
/// # Examples
///
/// ```text
/// kash listen 4444
/// kash listen 4444 -s linux
/// kash ps
/// kash ps -q
/// kash exec <session-id> whoami
/// kash exec <session-id> --format json cat /etc/passwd
/// kash inspect <session-id>
/// kash kill <session-id>
/// ```
#[derive(Parser, Debug)]
#[command(
    name = "kash",
    about = "Reverse shell handler with session management",
    version,
    subcommand_required = true,
    arg_required_else_help = true,
)]
pub struct Cli {
    /// Skip the background update check (also: KASH_NO_UPDATE_CHECK=1).
    #[arg(long, global = true)]
    pub no_update_check: bool,

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

    /// Re-attach an interactive terminal to a detached session.
    Attach(AttachArgs),

    /// Upload a local file to a running session's remote system.
    Upload(UploadArgs),

    /// Download a file from a running session's remote system.
    Download(DownloadArgs),

    /// Run a script module in a running session.
    Run(RunArgs),

    /// List available script modules.
    Modules(ModulesArgs),

    /// Connect to a remote SSH server and manage it like a session.
    Ssh(SshArgs),

    /// Connect to a Windows host over WinRM and manage it like a session.
    Winrm(WinrmArgs),
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

    /// Target shell type: auto | linux | windows.
    #[arg(short = 's', long, default_value = "auto")]
    pub shell: ShellTypeArg,

    /// Session identity and backgrounding (see [`SessionArgs`]).
    #[command(flatten)]
    pub common: SessionArgs,
}

impl ListenArgs {
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
    ///   kash exec <id> --cmd "python3 -c \"print('hello')\""
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

#[derive(ClapArgs, Debug)]
pub struct AttachArgs {
    /// Session ID to attach to.
    pub session: String,
}

#[derive(ClapArgs, Debug)]
pub struct UploadArgs {
    /// Session ID.
    pub session: String,
    /// Local file path to upload.
    pub local: String,
    /// Remote destination path (defaults to the local filename).
    pub remote: Option<String>,
}

#[derive(ClapArgs, Debug)]
pub struct DownloadArgs {
    /// Session ID.
    pub session: String,
    /// Remote file path to download.
    pub remote: String,
    /// Local destination path (defaults to the remote filename).
    pub local: Option<String>,
}

#[derive(ClapArgs, Debug)]
pub struct RunArgs {
    /// Module name (see `kash modules`).
    pub module: String,
    /// Session ID to run it in.
    pub session: String,
    /// Template values (`--set KEY=VAL`, repeatable) for `{{VAR}}` placeholders.
    #[arg(long = "set", value_name = "KEY=VAL")]
    pub set: Vec<String>,
    /// Skip the confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Output format: text (default) or json.
    #[arg(long, default_value = "text")]
    pub format: OutputFormatArg,
}

impl RunArgs {
    pub fn output_format(&self) -> OutputFormat {
        self.format.0
    }
}

#[derive(ClapArgs, Debug)]
pub struct ModulesArgs {
    /// Output as JSON array.
    #[arg(long)]
    pub json: bool,
}

/// Flags shared by every transport subcommand (`listen`, `ssh`, `winrm`,
/// …): session identity and backgrounding. Flattened into each `*Args`
/// struct so all transports spell session management identically.
///
/// ```text
/// kash listen 4444 --session monkey -d
/// kash ssh root@h --session monkey -d
/// ```
#[derive(ClapArgs, Debug)]
pub struct SessionArgs {
    /// Override the random session ID with a custom value (useful for scripts).
    #[arg(long)]
    pub session: Option<String>,

    /// Background the session immediately — the connection keeps running in a
    /// background process. Reconnect interactively with `attach <session-id>`.
    /// Unix only.
    #[arg(short = 'd', long)]
    pub daemon: bool,

    /// Internal flag: session is running headless (no local terminal).
    /// Set automatically by --daemon; do not pass directly.
    #[arg(long, hide = true)]
    pub headless: bool,
}

#[derive(ClapArgs, Debug)]
pub struct SshArgs {
    /// Remote target as [user@]host.
    pub target: String,

    /// SSH port (default: 22).
    #[arg(short = 'p', long, default_value_t = 22)]
    pub port: u16,

    /// Identity (private key) file for publickey auth.
    #[arg(short = 'i', long)]
    pub identity: Option<String>,

    /// Prompt for password auth (also used as fallback when agent/keys fail).
    #[arg(long)]
    pub password: bool,

    /// Reject unknown host keys instead of trusting on first use.
    /// Mismatched (changed) keys are always rejected.
    #[arg(long)]
    pub strict_host_key_checking: bool,

    /// Target shell type: auto | linux | windows.
    #[arg(short = 's', long, default_value = "auto")]
    pub shell: ShellTypeArg,

    /// Session identity and backgrounding (see [`SessionArgs`]).
    #[command(flatten)]
    pub common: SessionArgs,
}

impl SshArgs {
    pub fn shell_type(&self) -> ShellType {
        self.shell.0
    }
}

#[derive(ClapArgs, Debug)]
pub struct WinrmArgs {
    /// Remote target as [user@]host.
    pub target: String,

    /// WinRM port (default: 5985, or 5986 with --ssl).
    #[arg(short = 'p', long)]
    pub port: Option<u16>,

    /// Use HTTPS transport (default port 5986).
    #[arg(long)]
    pub ssl: bool,

    /// Skip TLS certificate verification (self-signed server certs).
    #[arg(long)]
    pub insecure: bool,

    /// Auth scheme: basic | ntlm (default: ntlm).
    #[arg(long, default_value = "ntlm")]
    pub auth: crate::winrm::WinRmAuthArg,

    /// Remote shell: powershell | cmd (default: powershell).
    #[arg(long, default_value = "powershell")]
    pub shell: crate::winrm::WinRmShellArg,

    /// Prompt for password auth (also used when no other method is configured).
    #[arg(long)]
    pub password: bool,

    /// Session identity and backgrounding (see [`SessionArgs`]).
    #[command(flatten)]
    pub common: SessionArgs,
}

impl WinrmArgs {
    /// Effective port: explicit `--port` wins, otherwise 5986/5985 by `--ssl`.
    pub fn resolve_port(&self) -> u16 {
        crate::winrm::resolve_port(self.port, self.ssl)
    }

    /// WinRM sessions always run a Windows shell (line-editor mode, no PTY).
    pub fn shell_type(&self) -> ShellType {
        ShellType::Windows
    }
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
        assert_eq!(a.shell_type(), ShellType::Auto);
        assert!(a.common.session.is_none());
        assert!(!a.common.daemon);
        assert!(!a.common.headless);
    }

    #[test]
    fn listen_daemon_flag() {
        let cli = Cli::try_parse_from(["sh", "listen", "9001", "-d"]).unwrap();
        let Command::Listen(a) = cli.command else { panic!() };
        assert!(a.common.daemon);
        assert!(!a.common.headless);
    }

    #[test]
    fn listen_headless_hidden_flag() {
        let cli = Cli::try_parse_from(["sh", "listen", "9001", "--headless"]).unwrap();
        let Command::Listen(a) = cli.command else { panic!() };
        assert!(a.common.headless);
    }

    #[test]
    fn listen_all_options() {
        let cli = Cli::try_parse_from([
            "sh", "listen", "9001", "-l", "10.0.0.5", "-s", "linux",
            "--session", "abcd1234",
        ])
        .unwrap();
        let Command::Listen(a) = cli.command else { panic!() };
        assert_eq!(a.port, 9001);
        assert_eq!(a.listen, "10.0.0.5");
        assert_eq!(a.shell_type(), ShellType::Linux);
        assert_eq!(a.common.session.as_deref(), Some("abcd1234"));
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
    fn attach_parses() {
        let cli = Cli::try_parse_from(["sh", "attach", "abc12345"]).unwrap();
        let Command::Attach(a) = cli.command else { panic!() };
        assert_eq!(a.session, "abc12345");
    }

    #[test]
    fn run_minimal() {
        let cli = Cli::try_parse_from(["sh", "run", "enum-users", "abc12345"]).unwrap();
        let Command::Run(a) = cli.command else { panic!() };
        assert_eq!(a.module, "enum-users");
        assert_eq!(a.session, "abc12345");
        assert!(a.set.is_empty());
        assert!(!a.yes);
        assert_eq!(a.output_format(), OutputFormat::Text);
    }

    #[test]
    fn run_all_options() {
        let cli = Cli::try_parse_from([
            "sh", "run", "s3-enum", "abc12345",
            "--set", "BUCKET=foo", "--set", "REGION=us-east-1",
            "--yes", "--format", "json",
        ])
        .unwrap();
        let Command::Run(a) = cli.command else { panic!() };
        assert_eq!(a.set, vec!["BUCKET=foo", "REGION=us-east-1"]);
        assert!(a.yes);
        assert_eq!(a.output_format(), OutputFormat::Json);
    }

    #[test]
    fn modules_parses() {
        let cli = Cli::try_parse_from(["sh", "modules"]).unwrap();
        let Command::Modules(a) = cli.command else { panic!() };
        assert!(!a.json);
        let cli = Cli::try_parse_from(["sh", "modules", "--json"]).unwrap();
        let Command::Modules(a) = cli.command else { panic!() };
        assert!(a.json);
    }

    #[test]
    fn no_subcommand_fails() {
        assert!(Cli::try_parse_from(["sh"]).is_err());
    }

    #[test]
    fn invalid_shell_type() {
        assert!(Cli::try_parse_from(["sh", "listen", "4444", "-s", "macos"]).is_err());
    }

    #[test]
    fn removed_short_flag_rejected() {
        // `-o` used to select obfuscation (removed) — it must now fail.
        assert!(Cli::try_parse_from(["sh", "listen", "4444", "-o", "light"]).is_err());
        assert!(Cli::try_parse_from(["sh", "ssh", "h", "-o", "light"]).is_err());
    }

    #[test]
    fn listen_missing_port_fails() {
        assert!(Cli::try_parse_from(["sh", "listen"]).is_err());
    }

    #[test]
    fn ssh_minimal() {
        let cli = Cli::try_parse_from(["sh", "ssh", "root@10.0.0.5"]).unwrap();
        let Command::Ssh(a) = cli.command else { panic!() };
        assert_eq!(a.target, "root@10.0.0.5");
        assert_eq!(a.port, 22);
        assert!(a.identity.is_none());
        assert!(!a.password);
        assert!(!a.strict_host_key_checking);
        assert!(!a.common.daemon);
        assert!(!a.common.headless);
        assert_eq!(a.shell_type(), ShellType::Auto);
    }

    #[test]
    fn ssh_all_options() {
        let cli = Cli::try_parse_from([
            "sh", "ssh", "kam@host", "-p", "2222", "-i", "/tmp/key",
            "--password", "--strict-host-key-checking", "-s", "linux",
            "--session", "monkey", "-d",
        ])
        .unwrap();
        let Command::Ssh(a) = cli.command else { panic!() };
        assert_eq!(a.target, "kam@host");
        assert_eq!(a.port, 2222);
        assert_eq!(a.identity.as_deref(), Some("/tmp/key"));
        assert!(a.password);
        assert!(a.strict_host_key_checking);
        assert!(a.common.daemon);
        assert_eq!(a.common.session.as_deref(), Some("monkey"));
    }

    #[test]
    fn ssh_missing_target_fails() {
        assert!(Cli::try_parse_from(["sh", "ssh"]).is_err());
    }

    #[test]
    fn winrm_minimal() {
        let cli = Cli::try_parse_from(["sh", "winrm", "Admin@10.0.0.5"]).unwrap();
        let Command::Winrm(a) = cli.command else { panic!() };
        assert_eq!(a.target, "Admin@10.0.0.5");
        assert_eq!(a.resolve_port(), 5985);
        assert!(!a.ssl);
        assert!(!a.insecure);
        assert!(!a.password);
        assert!(!a.common.daemon);
        assert_eq!(a.shell_type(), ShellType::Windows);
    }

    #[test]
    fn winrm_ssl_defaults_to_5986() {
        let cli = Cli::try_parse_from(["sh", "winrm", "h", "--ssl"]).unwrap();
        let Command::Winrm(a) = cli.command else { panic!() };
        assert_eq!(a.resolve_port(), 5986);
    }

    #[test]
    fn winrm_explicit_port_wins_over_ssl_default() {
        let cli = Cli::try_parse_from(["sh", "winrm", "h", "--ssl", "-p", "55985"]).unwrap();
        let Command::Winrm(a) = cli.command else { panic!() };
        assert_eq!(a.resolve_port(), 55985);
    }

    #[test]
    fn winrm_all_options() {
        let cli = Cli::try_parse_from([
            "sh", "winrm", "Admin@host", "-p", "5986", "--ssl", "--insecure",
            "--auth", "basic", "--shell", "cmd", "--password",
            "--session", "orca", "-d",
        ])
        .unwrap();
        let Command::Winrm(a) = cli.command else { panic!() };
        assert!(a.ssl);
        assert!(a.insecure);
        assert!(a.password);
        assert!(a.common.daemon);
        assert_eq!(a.common.session.as_deref(), Some("orca"));
    }

    #[test]
    fn winrm_missing_target_fails() {
        assert!(Cli::try_parse_from(["sh", "winrm"]).is_err());
    }

    #[test]
    fn winrm_invalid_auth_and_shell_fail() {
        assert!(Cli::try_parse_from(["sh", "winrm", "h", "--auth", "kerberos"]).is_err());
        assert!(Cli::try_parse_from(["sh", "winrm", "h", "--shell", "bash"]).is_err());
    }
}
