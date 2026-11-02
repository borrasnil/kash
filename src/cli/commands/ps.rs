//! `kash ps` — list active shell sessions.

use crate::agent;
use crate::cli::PsArgs;
use crate::util::json_str;

/// List active sessions as an ID-only list (`-q`), JSON array (`--json`),
/// or a human-readable table. Dead sessions (socket files without a serving
/// process, e.g. after a crash/reboot) are pruned and not listed.
pub async fn cmd_ps(args: PsArgs) -> anyhow::Result<()> {
    let sessions = agent::list_sessions_alive().await;

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
                "{{\"id\":{},\"peer\":{},\"user\":{},\"host\":{},\"transport\":{},\"shell\":{}}}",
                json_str(&s.id),
                json_str(&s.peer),
                json_str(&s.user),
                json_str(&s.host),
                json_str(&s.transport),
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
    const W_TRANSPORT: usize = 9;

    println!(
        "\x1b[2m{:<W_ID$}  {:<W_PEER$}  {:<W_IDENTITY$}  {:<W_TRANSPORT$}  SHELL\x1b[0m",
        "SESSION", "PEER", "IDENTITY", "TRANSPORT"
    );
    println!("{}", "\x1b[2m─\x1b[0m".repeat(W_ID + W_PEER + W_IDENTITY + W_TRANSPORT + 20));

    for s in &sessions {
        let identity = if s.user == "?" && s.host == "?" {
            "?".to_string()
        } else {
            format!("{}@{}", s.user, s.host)
        };
        println!(
            "\x1b[1;36m{:<W_ID$}\x1b[0m  \x1b[1;37m{:<W_PEER$}\x1b[0m  {:<W_IDENTITY$}  {:<W_TRANSPORT$}  {}",
            s.id, s.peer, identity, s.transport, s.shell,
        );
    }

    Ok(())
}
