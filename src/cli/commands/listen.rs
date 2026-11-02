//! `kash listen` — start a listener and wait for an incoming reverse shell.

use std::io::Write as _;

use crate::cli::ListenArgs;
use crate::prompt;
use crate::session::id::generate_session_id;
use crate::session::run_session;
use crate::transport::{BoxReader, BoxWriter, Transport, TransportCaps, TransportInfo, TransportKind};

/// Inbound reverse-shell transport: binds, accepts one connection, and
/// yields the shell stream. Needs the `pty.spawn` upgrade, so it reports
/// [`TransportCaps::reverse`].
pub struct ReverseTransport {
    /// Bind address (e.g. `0.0.0.0`).
    pub bind: String,
    /// TCP port to listen on.
    pub port: u16,
}

impl Transport for ReverseTransport {
    /// Bind, accept one inbound connection, and split it into boxed halves.
    async fn connect(&self) -> anyhow::Result<(BoxReader, BoxWriter, TransportInfo)> {
        let (stream, peer_addr) = crate::listen::listen(&self.bind, self.port)
            .await
            .map_err(|e| anyhow::anyhow!("{e:#}"))?;
        let peer = peer_addr.to_string();
        let (reader, writer) = stream.into_split();
        let info = TransportInfo::new(TransportKind::Reverse, &peer, TransportCaps::reverse());
        Ok((Box::new(reader), Box::new(writer), info))
    }
}

/// `listen` front door: pick a session ID, optionally spawn a detached
/// daemon child (Unix `-d`), then accept one reverse connection and hand it
/// to [`run_session`]. A `--shell windows` request marks the dialect so
/// marker framing speaks PowerShell.
pub async fn cmd_listen(args: ListenArgs) -> anyhow::Result<()> {
    let session_id = args
        .common
        .session
        .clone()
        .unwrap_or_else(generate_session_id);

    // Daemon mode: spawn a headless child, print the session info, and return.
    // The child handles the TCP accept and runs the session without a terminal.
    if args.common.daemon {
        let extra = vec![
            args.port.to_string(),
            "--listen".to_string(),
            args.listen.clone(),
            "--shell".to_string(),
            args.shell.to_string(),
        ];
        return crate::transport::spawn_daemon(
            "listen",
            &extra,
            &session_id,
            &prompt::startup_banner_daemon(&args.listen, args.port, args.shell_type(), &session_id),
        );
    }

    if !args.common.headless {
        print!(
            "{}",
            prompt::startup_banner(
                &args.listen,
                args.port,
                args.shell_type(),
                &session_id,
            )
        );
        let _ = std::io::stdout().flush();
    }

    let transport = ReverseTransport {
        bind: args.listen.clone(),
        port: args.port,
    };
    let (reader, writer, info) = transport.connect().await?;
    let info = if args.shell_type() == crate::cli::ShellType::Windows {
        info.with_shell(crate::transport::RemoteShell::Powershell)
    } else {
        info
    };

    run_session(
        reader,
        writer,
        args.shell_type(),
        &session_id,
        args.common.headless,
        info,
    )
    .await?;

    Ok(())
}
