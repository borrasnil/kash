//! `kash ssh` — connect to a remote SSH server and manage it like a session.
//!
//! Target/flag syntax mirrors `ssh(1)`: `kash ssh [-p port] [-i key]...
//! [-l user] [-F config] [-o opt]... [user@]target`, resolved against the
//! ssh_config parser ([`crate::ssh::config`]) and a default-key probe.
//! `kash ssh config <target>` prints the resolved values (ssh -G style)
//! without connecting.

use std::io::Write as _;
use std::path::PathBuf;

use crate::cli::{SshArgs, SshSub};
use crate::prompt;
use crate::session::id::generate_session_id;
use crate::session::run_session;
use crate::ssh::{self, config, ConnectOptions, ResolvedConnect};
use crate::transport::Transport;

/// CLI-level values collected from ssh(1)-style flags (`-l`, `-p`, `-i`,
/// `-o User=` / `-o Port=` / `-o IdentityFile=` / `-o
/// StrictHostKeyChecking=`). All of these sit above ssh_config in ssh's
/// precedence order.
#[derive(Debug, Default)]
struct CliOverrides {
    user: Option<String>,
    port: Option<u16>,
    identity: Vec<String>,
    strict_host_key_checking: Option<bool>,
}

impl CliOverrides {
    fn from_args(args: &SshArgs) -> Self {
        let mut o = Self {
            user: args.login.clone(),
            port: args.port,
            identity: args.identity.clone(),
            strict_host_key_checking: None,
        };
        for line in &args.option {
            apply_option_line(line, &mut o);
        }
        o
    }
}

/// Parse one `-o Key=Value` (ssh also accepts bare `Key Value`; single-token
/// form is what the CLI can carry). Only keywords kash implements are
/// handled; unknown ones warn and are skipped, mirroring ssh diagnostics.
fn apply_option_line(line: &str, o: &mut CliOverrides) {
    let (key, value) = match line.split_once('=') {
        Some((k, v)) => (k.trim(), Some(v)),
        // Bare keyword (e.g. `StrictHostKeyChecking`) defaults to yes.
        None => (line.trim(), None),
    };
    let key = key.to_ascii_lowercase();
    match (key.as_str(), value) {
        ("stricthostkeychecking", v) => {
            let raw = v.unwrap_or("yes").trim().to_ascii_lowercase();
            let strict = !(raw == "no" || raw == "false");
            o.strict_host_key_checking = Some(strict);
        }
        ("user", Some(v)) => {
            let v = v.trim();
            if !v.is_empty() {
                o.user = Some(v.to_string());
            }
        }
        ("port", Some(v)) => o.port = v.trim().parse::<u16>().ok().filter(|p| *p > 0),
        ("identityfile", Some(v)) => {
            let v = v.trim();
            if !v.is_empty() {
                o.identity.push(v.to_string());
            }
        }
        _ => eprintln!("kash: warning: ignoring unsupported -o option '{line}'"),
    }
}

/// Load the ssh_config the args point at (`-F` beats default
/// `~/.ssh/config`) and resolve everything for `target` — the shared code
/// path behind both connect and `kash ssh config`.
/// `overrides` carries the CLI-level values (`-l`, `-p`, `-i`, `-o ...`)
/// that sit above ssh_config.
fn resolve_target(
    args: &SshArgs,
    overrides: &CliOverrides,
    target_str: &str,
) -> anyhow::Result<ResolvedConnect> {
    let target = ssh::SshTarget::parse(target_str)?;

    let (config_path, cfg) = if let Some(path) = &args.config {
        let pb = PathBuf::from(ssh::expand_tilde(path));
        let cfg = config::load(&pb, &target.host)?;
        (Some(pb), cfg)
    } else {
        match config::default_path() {
            Some(pb) => (Some(pb.clone()), config::load(&pb, &target.host)?),
            None => (None, config::SshConfig::for_target(&target.host)),
        }
    };

    let mut resolved = ssh::resolve_connect(
        &target,
        overrides.user.as_deref(),
        overrides.port,
        &overrides.identity,
        &cfg,
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .as_deref(),
    );
    resolved.config_path = config_path;
    Ok(resolved)
}

pub async fn cmd_ssh(args: SshArgs) -> anyhow::Result<()> {
    let overrides = CliOverrides::from_args(&args);

    // `kash ssh config <target>` — print resolved values, no connection.
    if let Some(SshSub::Config { target }) = &args.sub {
        let resolved = resolve_target(&args, &overrides, target)?;
        print!("{}", resolved.dump());
        let _ = std::io::stdout().flush();
        return Ok(());
    }

    let target_str = args
        .target
        .clone()
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("usage: kash ssh [options] [user@]target"))?;

    let session_id = args
        .common
        .session
        .clone()
        .unwrap_or_else(generate_session_id);

    // Resolution runs before both branches so the daemon re-spawns with the
    // same values the foreground path would have used.
    let resolved = resolve_target(&args, &overrides, &target_str)?;
    let shell_type = args.shell_type();

    if args.common.daemon {
        // Re-inject the original ssh(1)-style flags; the child re-runs the
        // same resolution and lands on identical values (config dedup keeps
        // identity lists stable across the re-parse).
        let mut extra = vec![target_str, "--shell".to_string(), args.shell.to_string()];
        if let Some(port) = args.port {
            extra.push("--port".to_string());
            extra.push(port.to_string());
        }
        for identity in &args.identity {
            extra.push("--identity".to_string());
            extra.push(identity.clone());
        }
        if let Some(user) = &args.login {
            extra.push("--login".to_string());
            extra.push(user.clone());
        }
        if let Some(cfg) = &args.config {
            extra.push("--config".to_string());
            extra.push(cfg.clone());
        }
        for option in &args.option {
            extra.push("--option".to_string());
            extra.push(option.clone());
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
                &format!("{}@{}", resolved.user, resolved.host),
                resolved.port,
                shell_type,
                &session_id,
            ),
        );
    }

    if !args.common.headless {
        print!(
            "{}",
            prompt::startup_banner_ssh(
                &resolved.user,
                &resolved.host,
                resolved.port,
                shell_type,
                &session_id
            )
        );
        let _ = std::io::stdout().flush();
    }

    let transport = ssh::SshTransport {
        host: resolved.host.clone(),
        port: resolved.port,
        user: resolved.user.clone(),
        opts: ConnectOptions {
            identity_files: resolved.identity_files.clone(),
            password: args.password,
            strict_host_key_checking: args.strict_host_key_checking
                || overrides.strict_host_key_checking == Some(true),
            normalize_shell: shell_type != crate::cli::ShellType::Windows,
        },
    };
    let (reader, writer, info) = transport.connect().await?;
    // Windows SSH targets speak PowerShell by default; marker framing and
    // identity probing follow the dialect, not the transport.
    let info = if shell_type == crate::cli::ShellType::Windows {
        info.with_shell(crate::transport::RemoteShell::Powershell)
    } else {
        info
    };

    run_session(
        reader,
        writer,
        shell_type,
        &session_id,
        args.common.headless,
        info,
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_line_parsing() {
        let mut o = CliOverrides::default();
        apply_option_line("StrictHostKeyChecking=yes", &mut o);
        assert_eq!(o.strict_host_key_checking, Some(true));
        apply_option_line("StrictHostKeyChecking", &mut o);
        assert_eq!(o.strict_host_key_checking, Some(true));
        apply_option_line("User=parallels", &mut o);
        assert_eq!(o.user.as_deref(), Some("parallels"));
        apply_option_line("Port=2222", &mut o);
        assert_eq!(o.port, Some(2222));
        apply_option_line("IdentityFile=~/.ssh/x1", &mut o);
        assert_eq!(o.identity, vec!["~/.ssh/x1".to_string()]);
    }

    #[test]
    fn option_line_unknown_warns_skips() {
        let mut o = CliOverrides::default();
        apply_option_line("ProxyJump=bastion", &mut o);
        assert_eq!(o.port, None);
        assert_eq!(o.user, None);
    }

    #[test]
    fn option_line_port_rejects_junk() {
        let mut o = CliOverrides::default();
        apply_option_line("Port=notanumber", &mut o);
        apply_option_line("Port=0", &mut o);
        assert_eq!(o.port, None);
    }
}
