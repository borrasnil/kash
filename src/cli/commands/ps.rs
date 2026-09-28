//! `kash ps` — list active shell sessions.

use crate::agent;
use crate::cli::PsArgs;
use crate::util::json_str;

pub fn cmd_ps(args: PsArgs) -> anyhow::Result<()> {
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
