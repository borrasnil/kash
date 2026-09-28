//! `kash kill` — terminate a running session gracefully.

use anyhow::Context;

use crate::agent;
use crate::cli::KillArgs;

pub async fn cmd_kill(args: KillArgs) -> anyhow::Result<()> {
    agent::kill_session(&args.session)
        .await
        .with_context(|| format!("failed to kill session '{}'", args.session))?;

    println!("\x1b[1;32m[+]\x1b[0m session '{}' terminated", args.session);
    Ok(())
}
