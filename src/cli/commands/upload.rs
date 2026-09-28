//! `kash upload` — upload a local file to a running session's remote system.

use std::io::Write as _;

use anyhow::Context;

use crate::agent;
use crate::cli::UploadArgs;

pub async fn cmd_upload(args: UploadArgs) -> anyhow::Result<()> {
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
