//! `kash download` — download a file from a running session's remote system.

use std::io::Write as _;

use anyhow::Context;

use crate::agent;
use crate::cli::DownloadArgs;

pub async fn cmd_download(args: DownloadArgs) -> anyhow::Result<()> {
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
