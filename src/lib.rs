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

#[cfg(unix)]
extern crate libc;

use std::io::{self, Write as _};
use std::sync::LazyLock;

use anyhow::Context;
use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use rand::seq::SliceRandom;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        Command::Attach(args) => cmd_attach(args).await,
        Command::Upload(args) => cmd_upload(args).await,
        Command::Download(args) => cmd_download(args).await,
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

    // Daemon mode: spawn a headless child, print the session info, and return.
    // The child handles the TCP accept and runs the session without a terminal.
    if args.daemon {
        return spawn_daemon(&args, &session_id);
    }

    if !args.headless {
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
    }

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
        args.headless,
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// daemon spawner
// ---------------------------------------------------------------------------

/// Spawn a background (headless) worker with the same listener config, then
/// return immediately — the calling process exits and the terminal is freed.
#[cfg(unix)]
fn spawn_daemon(args: &cli::ListenArgs, session_id: &str) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    print!(
        "{}",
        prompt::startup_banner_daemon(
            &args.listen,
            args.port,
            args.obfuscation_level(),
            args.shell_type(),
            session_id,
        )
    );
    let _ = std::io::stdout().flush();

    let exe = std::env::current_exe().context("cannot resolve current executable path")?;
    let null = std::fs::File::open("/dev/null").context("cannot open /dev/null")?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("listen")
        .arg(args.port.to_string())
        .arg("--listen").arg(&args.listen)
        .arg("--obfuscation").arg(args.obfuscation.to_string())
        .arg("--shell").arg(args.shell.to_string())
        .arg("--session").arg(session_id)
        .arg("--headless")
        .stdin(null.try_clone()?)
        .stdout(null.try_clone()?)
        .stderr(null);

    // Detach from the controlling terminal so SIGHUP on terminal close
    // doesn't kill the background session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    cmd.spawn().context("failed to spawn daemon process")?;
    Ok(())
}

#[cfg(not(unix))]
fn spawn_daemon(_args: &cli::ListenArgs, _session_id: &str) -> anyhow::Result<()> {
    anyhow::bail!("--daemon (-d) is not supported on this platform (Unix only)")
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
             Simple:   kash exec <session> whoami\n\
             Complex:  kash exec <session> --cmd \"python3 -c \\\"print('hello')\\\"\""
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

/// Popular, easy-to-type animal names used for auto-generated session IDs.
static ANIMAL_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    vec![
        "monkey", "tiger", "lion", "bear", "wolf", "fox", "deer", "eagle",
        "hawk", "crow", "owl", "snake", "zebra", "horse", "goat", "sheep",
        "camel", "moose", "elk", "bison", "rat", "rabbit", "otter", "beaver",
        "badger", "whale", "shark", "dolphin", "turtle", "frog", "lizard",
        "hyena", "leopard", "panda", "koala", "giraffe", "hippo", "crab",
        "squid", "penguin",
    ]
});

/// True if `id` is already assigned to a currently-active session (same scan as `ps`).
fn session_id_in_use(id: &str) -> bool {
    agent::list_sessions().iter().any(|s| s.id == id)
}

fn generate_session_id() -> String {
    let mut rng = rand::thread_rng();
    let mut candidates: Vec<&str> = ANIMAL_NAMES.clone();
    candidates.shuffle(&mut rng);
    if let Some(name) = candidates.into_iter().find(|n| !session_id_in_use(n)) {
        return name.to_string();
    }
    let base = ANIMAL_NAMES.choose(&mut rng).unwrap();
    (2..)
        .map(|i| format!("{base}{i}"))
        .find(|candidate| !session_id_in_use(candidate))
        .unwrap()
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
// upload / download (via session IPC)
// ---------------------------------------------------------------------------

async fn cmd_upload(args: cli::UploadArgs) -> anyhow::Result<()> {
    if !std::path::Path::new(&args.local).exists() {
        anyhow::bail!("local file not found: {}", args.local);
    }
    let remote = args.remote.unwrap_or_else(|| {
        std::path::Path::new(&args.local)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&args.local)
            .to_string()
    });
    let cmd = format!("{}{}\x00{}", agent::UPLOAD_CMD_PREFIX, args.local, remote);
    let (output, exit_code) = agent::send_command(&args.session, &cmd)
        .await
        .with_context(|| format!("failed to reach session '{}'", args.session))?;
    if !output.is_empty() {
        print!("{output}");
        let _ = std::io::stdout().flush();
    }
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

async fn cmd_download(args: cli::DownloadArgs) -> anyhow::Result<()> {
    let local = args.local.unwrap_or_else(|| {
        std::path::Path::new(&args.remote)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&args.remote)
            .to_string()
    });
    let cmd = format!("{}{}\x00{}", agent::DOWNLOAD_CMD_PREFIX, args.remote, local);
    let (output, exit_code) = agent::send_command(&args.session, &cmd)
        .await
        .with_context(|| format!("failed to reach session '{}'", args.session))?;
    if !output.is_empty() {
        print!("{output}");
        let _ = std::io::stdout().flush();
    }
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// attach
// ---------------------------------------------------------------------------

async fn cmd_attach(args: cli::AttachArgs) -> anyhow::Result<()> {
    let sock = agent::socket_path(&args.session);
    if !std::path::Path::new(&sock).exists() {
        anyhow::bail!("session '{}' not found", args.session);
    }

    let stream = tokio::net::UnixStream::connect(&sock).await.map_err(|_| {
        anyhow::anyhow!(
            "session '{}' not found — is kash running?",
            args.session
        )
    })?;

    // Announce ourselves as an interactive attach.
    let (mut sock_reader, mut sock_writer) = stream.into_split();
    sock_writer
        .write_all(format!("{}\n", agent::ATTACH_CMD).as_bytes())
        .await?;

    let _raw = terminal::RawModeGuard::enable()?;
    let mut stdout = io::stdout();

    write!(
        stdout,
        "{}\r\n",
        prompt::banner_attach_connected(&args.session)
    )?;
    stdout.flush()?;

    // Immediately sync the remote PTY to our terminal size so the user doesn't
    // have to resize their window to get the correct geometry.
    if let Ok((w, h)) = crossterm::terminal::size() {
        let _ = sock_writer
            .write_all(format!("stty cols {w} rows {h}\r").as_bytes())
            .await;
        let _ = sock_writer.flush().await;
    }

    let mut events = EventStream::new();
    let mut sock_buf = vec![0u8; 4096];

    loop {
        tokio::select! {
            // Session output → our terminal.
            result = sock_reader.read(&mut sock_buf) => {
                let n = result?;
                if n == 0 {
                    write!(stdout, "\r\n{}\r\n", prompt::banner_attach_session_closed())?;
                    stdout.flush()?;
                    break;
                }
                stdout.write_all(&util::raw_normalize(&sock_buf[..n]))?;
                stdout.flush()?;
            }

            // Keyboard → session.
            event_result = events.next() => {
                match event_result {
                    Some(Ok(Event::Resize(w, h))) => {
                        // Sync the remote PTY size.
                        let cmd = format!("stty cols {w} rows {h}\r");
                        let _ = sock_writer.write_all(cmd.as_bytes()).await;
                        let _ = sock_writer.flush().await;
                    }
                    Some(Ok(Event::Key(key))) => {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        // CTRL+Q / CTRL+] — detach from the session (close socket).
                        if ctrl && (key.code == KeyCode::Char('q') || key.code == KeyCode::Char(']')) {
                            write!(stdout, "\r\n{}\r\n", prompt::banner_attach_detached())?;
                            stdout.flush()?;
                            break;
                        }
                        let bytes = util::key_to_bytes(key);
                        if !bytes.is_empty() {
                            sock_writer.write_all(&bytes).await?;
                            sock_writer.flush().await?;
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        sock_writer.write_all(text.as_bytes()).await?;
                        sock_writer.flush().await?;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_an_animal() {
        let id = generate_session_id();
        let base = id.trim_end_matches(|c: char| c.is_ascii_digit());
        assert!(ANIMAL_NAMES.contains(&base), "unexpected session id: {id}");
        assert!(!session_id_in_use(&id));
    }

    #[test]
    fn session_id_in_use_detects_active_session() {
        assert!(!session_id_in_use("__test_in_use__"));
        std::fs::write(agent::socket_path("__test_in_use__"), b"").unwrap();
        assert!(session_id_in_use("__test_in_use__"));
        let _ = std::fs::remove_file(agent::socket_path("__test_in_use__"));
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
