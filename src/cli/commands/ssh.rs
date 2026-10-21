//! `kash ssh` — connect to a remote SSH server and manage it like a session.

use std::io::Write as _;

use crate::cli::SshArgs;
use crate::prompt;
use crate::session::id::generate_session_id;
use crate::session::run_session;
use crate::ssh::{self, ConnectOptions};
use crate::transport::Transport;

pub async fn cmd_ssh(args: SshArgs) -> anyhow::Result<()> {
    let session_id = args
        .common
        .session
        .clone()
        .unwrap_or_else(generate_session_id);

    if args.common.daemon {
        let mut extra = vec![
            args.target.clone(),
            "--port".to_string(),
            args.port.to_string(),
            "--shell".to_string(),
            args.shell.to_string(),
        ];
        if let Some(identity) = &args.identity {
            extra.push("--identity".to_string());
            extra.push(identity.clone());
        }
        if args.password {
            extra.push("--password".to_string());
        }
        if args.strict_host_key_checking {
            extra.push("--strict-host-key-checking".to_string());
        }
        return crate::transport::spawn_daemon(
            "ssh",
            &extra,
            &session_id,
            &prompt::startup_banner_ssh_daemon(
                &args.target,
                args.port,
                args.shell_type(),
                &session_id,
            ),
        );
    }

    let default_user = ssh::default_local_user();
    let target = ssh::SshTarget::parse(&args.target, &default_user)?;

    if !args.common.headless {
        print!(
            "{}",
            prompt::startup_banner_ssh(
                &target.user,
                &target.host,
                args.port,
                args.shell_type(),
                &session_id,
            )
        );
        let _ = std::io::stdout().flush();
    }

    let transport = ssh::SshTransport {
        host: target.host.clone(),
        port: args.port,
        user: target.user.clone(),
        opts: ConnectOptions {
            identity: args.identity.clone(),
            password: args.password,
            strict_host_key_checking: args.strict_host_key_checking,
            normalize_shell: args.shell_type() != crate::cli::ShellType::Windows,
        },
    };
    let (reader, writer, info) = transport.connect().await?;
    // Windows SSH targets speak PowerShell by default; marker framing and
    // identity probing follow the dialect, not the transport.
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
