pub mod agent;
pub mod cli;
pub mod error;
pub mod line_editor;
pub mod listen;
pub mod obfuscation;
pub mod output;
pub mod prompt;
pub mod session;
pub mod terminal;
pub mod transfer;
pub mod util;

use std::io::Write as _;

use anyhow::Context;
use clap::Parser;
use rand::distributions::Alphanumeric;
use rand::Rng;

use crate::cli::{Cli, Command, OutputFormat};
use crate::listen::listen;
use crate::obfuscation::create_strategy;
use crate::session::run_session;

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Listen(args) => cmd_listen(args).await,
        Command::Ps(args) => cmd_ps(args),
        Command::Exec(args) => cmd_exec(args).await,
        Command::Inspect(args) => cmd_inspect(args),
        Command::Kill(args) => cmd_kill(args).await,
    }
}

// ---------------------------------------------------------------------------
// listen
// ---------------------------------------------------------------------------

async fn cmd_listen(args: cli::ListenArgs) -> anyhow::Result<()> {
    let session_id = args
        .session
        .clone()
        .unwrap_or_else(generate_session_id);

    print!(
        "{}",
        prompt::startup_banner(
            &args.listen,
            args.port,
            args.obfuscation_level(),
            args.shell_type(),
            &session_id,
        )
    );
    let _ = std::io::stdout().flush();

    let (stream, peer_addr) = listen(&args.listen, args.port)
        .await
        .with_context(|| format!("failed to listen on {}:{}", args.listen, args.port))?;

    let engine = create_strategy(args.obfuscation_level(), args.shell_type());

    run_session(
        stream,
        peer_addr,
        &*engine,
        args.obfuscation_level(),
        args.shell_type(),
        &session_id,
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// ps
// ---------------------------------------------------------------------------

fn cmd_ps(args: cli::PsArgs) -> anyhow::Result<()> {
    let sessions = agent::list_sessions();

    if sessions.is_empty() && !args.quiet && !args.json {
        println!("no active sessions");
        return Ok(());
    }

    if args.quiet {
        for s in &sessions {
            println!("{}", s.id);
        }
        return Ok(());
    }

    if args.json {
        print!("[");
        for (i, s) in sessions.iter().enumerate() {
            if i > 0 {
                print!(",");
            }
            print!(
                "{{\"id\":{},\"peer\":{},\"user\":{},\"host\":{},\"obfuscation\":{},\"shell\":{}}}",
                json_str(&s.id),
                json_str(&s.peer),
                json_str(&s.user),
                json_str(&s.host),
                json_str(&s.obfuscation),
                json_str(&s.shell),
            );
        }
        println!("]");
        return Ok(());
    }

    // Table output.
    const W_ID: usize = 10;
    const W_PEER: usize = 22;
    const W_IDENTITY: usize = 20;
    const W_OBF: usize = 12;

    println!(
        "\x1b[2m{:<W_ID$}  {:<W_PEER$}  {:<W_IDENTITY$}  {:<W_OBF$}  {}\x1b[0m",
        "SESSION", "PEER", "IDENTITY", "OBFUSCATION", "SHELL"
    );
    println!("{}", "\x1b[2m─\x1b[0m".repeat(W_ID + W_PEER + W_IDENTITY + W_OBF + 20));

    for s in &sessions {
        let identity = if s.user == "?" && s.host == "?" {
            "?".to_string()
        } else {
            format!("{}@{}", s.user, s.host)
        };
        println!(
            "\x1b[1;36m{:<W_ID$}\x1b[0m  \x1b[1;37m{:<W_PEER$}\x1b[0m  {:<W_IDENTITY$}  {:<W_OBF$}  {}",
            s.id, s.peer, identity, s.obfuscation, s.shell,
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// exec
// ---------------------------------------------------------------------------

async fn cmd_exec(args: cli::ExecArgs) -> anyhow::Result<()> {
    let cmd = args.command_str();
    if cmd.trim().is_empty() {
        anyhow::bail!(
            "exec requires a command\n\
             \n\
             Simple:   shell-handler exec <session> whoami\n\
             Complex:  shell-handler exec <session> --cmd \"python3 -c \\\"print('hello')\\\"\""
        );
    }

    let (output, exit_code) = agent::send_command(&args.session, &cmd)
        .await
        .with_context(|| format!("failed to reach session '{}'", args.session))?;

    match args.output_format() {
        OutputFormat::Text => {
            print!("{output}");
            let _ = std::io::stdout().flush();
            std::process::exit(exit_code);
        }
        OutputFormat::Json => {
            println!(
                "{{\"output\":{},\"exit_code\":{}}}",
                json_str(&output),
                exit_code,
            );
            std::process::exit(exit_code);
        }
    }
}

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

fn cmd_inspect(args: cli::InspectArgs) -> anyhow::Result<()> {
    let sock = agent::socket_path(&args.session);
    if !std::path::Path::new(&sock).exists() {
        anyhow::bail!("session '{}' not found", args.session);
    }

    let s = agent::read_session_info(&args.session);
    let identity = if s.user == "?" && s.host == "?" {
        "?".to_string()
    } else {
        format!("{}@{}", s.user, s.host)
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    println!();
    println!("  \x1b[2mSession     \x1b[0m  \x1b[1;36m{}\x1b[0m", s.id);
    println!("  \x1b[2mPeer        \x1b[0m  \x1b[1;37m{}\x1b[0m", s.peer);
    println!("  \x1b[2mIdentity    \x1b[0m  {}", identity);
    println!("  \x1b[2mObfuscation \x1b[0m  {}", s.obfuscation);
    println!("  \x1b[2mShell       \x1b[0m  {}", s.shell);
    println!();
    if s.started > 0 {
        println!(
            "  \x1b[2mStarted     \x1b[0m  {}  \x1b[2m({})\x1b[0m",
            unix_to_datetime_str(s.started),
            time_ago(s.started, now),
        );
    }
    println!("  \x1b[2mCommands    \x1b[0m  {}", s.cmd_count);
    if s.cmd_count > 0 && !s.last_cmd.is_empty() {
        let ago = if s.last_cmd_at > 0 {
            format!("  \x1b[2m({})\x1b[0m", time_ago(s.last_cmd_at, now))
        } else {
            String::new()
        };
        println!("  \x1b[2mLast cmd    \x1b[0m  {}{}", s.last_cmd, ago);
    }
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// kill
// ---------------------------------------------------------------------------

async fn cmd_kill(args: cli::KillArgs) -> anyhow::Result<()> {
    let sock = agent::socket_path(&args.session);
    if !std::path::Path::new(&sock).exists() {
        anyhow::bail!("session '{}' not found", args.session);
    }

    agent::kill_session(&args.session)
        .await
        .with_context(|| format!("failed to kill session '{}'", args.session))?;

    println!("\x1b[1;32m[+]\x1b[0m session '{}' terminated", args.session);
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format a Unix epoch as "YYYY-MM-DD HH:MM:SS UTC".
fn unix_to_datetime_str(secs: u64) -> String {
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

/// Human-readable elapsed time: "just now", "5m ago", "2h 3m ago", "3d ago".
fn time_ago(ts: u64, now: u64) -> String {
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

fn generate_session_id() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(8)
        .map(|c| (c as char).to_ascii_lowercase())
        .collect()
}

/// Produce a JSON-quoted string with minimal escaping.
fn json_str(s: &str) -> String {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_8_lowercase_alnum() {
        let id = generate_session_id();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() && !c.is_uppercase()));
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
}
