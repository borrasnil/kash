//! `kash inspect` — show detailed information about a session.

use crate::agent;
use crate::cli::InspectArgs;
use crate::util::{time_ago, unix_to_datetime_str};

pub fn cmd_inspect(args: InspectArgs) -> anyhow::Result<()> {
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
