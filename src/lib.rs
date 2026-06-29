pub mod cli;
pub mod error;
pub mod listen;
pub mod obfuscation;
pub mod output;
pub mod prompt;
pub mod session;
pub mod transfer;
pub mod util;

use anyhow::Context;
use clap::Parser;

use crate::cli::Args;
use crate::listen::listen;
use crate::obfuscation::create_strategy;
use crate::session::run_session;

pub async fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    let (stream, peer_addr) = listen(&args.listen, args.port)
        .await
        .with_context(|| format!("failed to listen on {}:{}", args.listen, args.port))?;

    let engine = create_strategy(args.obfuscation_level(), args.shell_type());

    run_session(stream, peer_addr, &*engine).await?;

    Ok(())
}
