//! `kash winrm` — connect to a Windows host over WinRM.
//!
//! Opens a persistent PowerShell (or `cmd`) REPL over WinRM SOAP and manages
//! it like any other session (`ps`/`exec`/`attach`/`upload`/`download`).
//! WinRM has no PTY, so sessions stay in handler/line-editor mode.
//!
//! Credentials resolve via [`crate::winrm::resolve_password`]: `--password`
//! prompts, else `WINRM_PASSWORD` (required for `-d`, inherited by the
//! background child), else an interactive prompt.

use std::io::Write as _;

use crate::cli::{ShellType, WinrmArgs};
use crate::prompt;
use crate::session::id::generate_session_id;
use crate::session::run_session;
use crate::transport::Transport;
use crate::winrm::{self, WinRmAuth, WinRmTarget};

pub async fn cmd_winrm(args: WinrmArgs) -> anyhow::Result<()> {
    let session_id = args
        .common
        .session
        .clone()
        .unwrap_or_else(generate_session_id);

    let default_user = crate::ssh::default_local_user();
    let target = WinRmTarget::parse(&args.target, &default_user)?;
    let port = args.resolve_port();

    if args.common.daemon {
        let mut extra = vec![
            args.target.clone(),
            "--port".to_string(),
            port.to_string(),
            "--auth".to_string(),
            args.auth.to_string(),
            "--shell".to_string(),
            args.shell.to_string(),
        ];
        if args.ssl {
            extra.push("--ssl".to_string());
        }
        if args.insecure {
            extra.push("--insecure".to_string());
        }
        if args.password {
            extra.push("--password".to_string());
        }
        return crate::transport::spawn_daemon(
            "winrm",
            &extra,
            &session_id,
            &prompt::startup_banner_ssh_daemon(
                &args.target,
                port,
                ShellType::Windows,
                &session_id,
            ),
        );
    }

    if args.auth.0 == WinRmAuth::Basic && !args.ssl {
        eprintln!("kash: warning: --auth basic without --ssl sends the password in cleartext");
    }

    if !args.common.headless {
        print!(
            "{}",
            prompt::startup_banner_ssh(
                &target.user,
                &target.host,
                port,
                ShellType::Windows,
                &session_id,
            )
        );
        let _ = std::io::stdout().flush();
    }

    let password = winrm::resolve_password(args.password)?;
    let transport = winrm::WinRmTransport {
        host: target.host.clone(),
        port,
        ssl: args.ssl,
        insecure: args.insecure,
        user: target.user.clone(),
        password,
        auth: args.auth.0,
        shell: args.shell.0,
    };
    let (reader, writer, info) = transport.connect().await?;

    run_session(
        reader,
        writer,
        ShellType::Windows,
        &session_id,
        args.common.headless,
        info,
    )
    .await?;

    Ok(())
}
