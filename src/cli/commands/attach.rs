//! `kash attach` — re-attach an interactive terminal to a detached session.

use std::io::{self, Write as _};

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::agent;
use crate::cli::AttachArgs;
use crate::prompt;
use crate::terminal::RawModeGuard;
use crate::util::{key_to_bytes, raw_normalize};

/// Open an `ATTACH_CMD` IPC connection to the session, enable raw mode, and
/// relay bytes bidirectionally until detach (CTRL+Q / CTRL+]), session close,
/// or EOF. Sends an initial `stty` so remote geometry matches this terminal.
pub async fn cmd_attach(args: AttachArgs) -> anyhow::Result<()> {
    let sock = agent::socket_path(&args.session);
    if !std::path::Path::new(&sock).exists() {
        anyhow::bail!("session '{}' not found", args.session);
    }
    if !agent::session_alive(&args.session).await {
        // Stale files from a killed/rebooted process — prune them like `ps`.
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(agent::info_path(&args.session));
        anyhow::bail!(
            "session '{}' is dead (its kash process exited — reboot or crash); \
             start a new session instead",
            args.session
        );
    }
    // Detach without `bg` leaves the serving process SIGSTOPed — it accepts
    // connections but stays silent. Wake it before anything else flows.
    agent::rescue_stopped(&args.session);

    let stream = tokio::net::UnixStream::connect(&sock).await.map_err(|_| {
        anyhow::anyhow!(
            "session '{}' not found — is kash running?",
            args.session
        )
    })?;

    // Announce ourselves as an interactive attach.
    let (mut sock_reader, mut sock_writer) = stream.into_split();
    sock_writer
        .write_all(format!("{}\n", agent::ATTACH_CMD).as_bytes())
        .await?;

    let _raw = RawModeGuard::enable()?;
    let mut stdout = io::stdout();

    write!(
        stdout,
        "{}\r\n",
        prompt::banner_attach_connected(&args.session)
    )?;
    stdout.flush()?;

    // Immediately sync the remote PTY to our terminal size so the user doesn't
    // have to resize their window to get the correct geometry.
    if let Ok((w, h)) = crossterm::terminal::size() {
        let _ = sock_writer
            .write_all(format!("stty cols {w} rows {h} 2>/dev/null\r").as_bytes())
            .await;
        let _ = sock_writer.flush().await;
    }

    let mut events = EventStream::new();
    let mut sock_buf = vec![0u8; 4096];
    // The server relays a `[-] session closed` banner before cutting the
    // stream. Whether that relay lands is racy (it can die on the same EOF),
    // so print our banner on EOF only when the relay didn't arrive.
    let mut saw_closed_banner = false;

    loop {
        tokio::select! {
            // Session output → our terminal.
            result = sock_reader.read(&mut sock_buf) => {
                let n = result?;
                if n == 0 {
                    if !saw_closed_banner {
                        write!(stdout, "\r\n{}\r\n", prompt::banner_attach_session_closed())?;
                        stdout.flush()?;
                    }
                    break;
                }
                if sock_buf[..n].windows(b"session closed".len()).any(|w| w == b"session closed") {
                    saw_closed_banner = true;
                }
                stdout.write_all(&raw_normalize(&sock_buf[..n]))?;
                stdout.flush()?;
            }

            // Keyboard → session.
            event_result = events.next() => {
                match event_result {
                    Some(Ok(Event::Resize(w, h))) => {
                        // Sync the remote PTY size.
                        let cmd = format!("stty cols {w} rows {h} 2>/dev/null\r");
                        let _ = sock_writer.write_all(cmd.as_bytes()).await;
                        let _ = sock_writer.flush().await;
                    }
                    Some(Ok(Event::Key(key))) => {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        // CTRL+Q / CTRL+] — detach from the session (close socket).
                        if ctrl && (key.code == KeyCode::Char('q') || key.code == KeyCode::Char(']')) {
                            write!(stdout, "\r\n{}\r\n", prompt::banner_attach_detached())?;
                            stdout.flush()?;
                            break;
                        }
                        let bytes = key_to_bytes(key);
                        if !bytes.is_empty() {
                            sock_writer.write_all(&bytes).await?;
                            sock_writer.flush().await?;
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        sock_writer.write_all(text.as_bytes()).await?;
                        sock_writer.flush().await?;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                }
            }
        }
    }

    Ok(())
}
