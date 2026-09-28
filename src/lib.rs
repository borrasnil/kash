//! kash — a reverse shell handler with Docker-style session management.
//!
//! This crate is organised into the following modules:
//!
//! * [`cli`] — argument parsing and command handlers
//! * [`session`] — the core interactive session event loop
//! * [`agent`] — Unix-socket IPC for external callers (agents, LLMs)
//! * [`obfuscation`] — command obfuscation strategies
//! * [`transfer`] — file upload/download via the remote shell
//! * [`terminal`] — raw mode guard and line editor
//! * [`output`] — ANSI/control-character cleaning
//! * [`prompt`] — banners and help text
//! * [`listen`] — TCP listener for incoming connections
//! * [`error`] — typed error enum
//! * [`util`] — shared utility functions

pub mod agent;
pub mod cli;
pub mod error;
pub mod listen;
pub mod obfuscation;
pub mod output;
pub mod prompt;
pub mod session;
pub mod terminal;
pub mod transfer;
pub mod util;

use clap::Parser;

use crate::cli::{Cli, Command};
use crate::cli::commands;

/// Parse CLI arguments and dispatch to the appropriate command handler.
pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Listen(args) => commands::listen::cmd_listen(args).await,
        Command::Ps(args) => commands::ps::cmd_ps(args),
        Command::Exec(args) => commands::exec::cmd_exec(args).await,
        Command::Inspect(args) => commands::inspect::cmd_inspect(args),
        Command::Kill(args) => commands::kill::cmd_kill(args).await,
        Command::Attach(args) => commands::attach::cmd_attach(args).await,
        Command::Upload(args) => commands::upload::cmd_upload(args).await,
        Command::Download(args) => commands::download::cmd_download(args).await,
    }
}
