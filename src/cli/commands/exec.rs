//! `kash exec` — execute a command in a running session.

use std::io::Write as _;

use anyhow::Context;

use crate::agent;
use crate::cli::{ExecArgs, OutputFormat};
use crate::util::json_str;

pub async fn cmd_exec(args: ExecArgs) -> anyhow::Result<()> {
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
