//! Outbound SSH client (`kash ssh`).
//!
//! Connects to a remote SSH server via `russh`, opens a session channel with
//! a PTY, and hands the resulting byte stream to the shared session loop.
//! Auth chain: explicit `--identity` file → ssh-agent → `--password` prompt.

use std::sync::Arc;

use anyhow::Context;
use tokio::io::AsyncWriteExt;

use crate::transport::{
    BoxReader, BoxWriter, Transport, TransportCaps, TransportInfo, TransportKind,
};

// ---------------------------------------------------------------------------
// Target parsing
// ---------------------------------------------------------------------------

/// Parsed `[user@]host`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub user: String,
    pub host: String,
}

impl SshTarget {
    /// Parse `[user@]host`. Empty user/host is an error.
    /// `default_user` is used when no `user@` prefix is present.
    pub fn parse(raw: &str, default_user: &str) -> anyhow::Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            anyhow::bail!("empty SSH target — expected [user@]host");
        }
        let (user, host) = match raw.rsplit_once('@') {
            Some((u, h)) => (u.to_string(), h.to_string()),
            None => (default_user.to_string(), raw.to_string()),
        };
        if user.is_empty() || host.is_empty() {
            anyhow::bail!("invalid SSH target '{raw}' — expected [user@]host");
        }
        Ok(Self { user, host })
    }

    pub fn display(&self, port: u16) -> String {
        format!("{}@{}:{}", self.user, self.host, port)
    }
}

pub fn default_local_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".to_string())
}

// ---------------------------------------------------------------------------
// russh client handler
// ---------------------------------------------------------------------------

/// Verifies the server host key against `~/.ssh/known_hosts`.
///
/// * Known key → accept silently.
/// * Changed key → reject (possible MITM), always.
/// * Unknown key → accept + learn (TOFU) unless `strict` rejects.
struct Client {
    host: String,
    port: u16,
    strict: bool,
}

fn known_hosts_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| std::path::PathBuf::from(h).join(".ssh").join("known_hosts"))
}

impl russh::client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = match server_public_key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => key,
            russh::keys::PublicKeyOrCertificate::Certificate(_) => {
                eprintln!(
                    "kash: warning: SSH host presented a certificate — skipping known_hosts check"
                );
                return Ok(true);
            }
        };
        let fingerprint = key.fingerprint(russh::keys::HashAlg::Sha256).to_string();
        let Some(path) = known_hosts_path() else {
            return Ok(true);
        };
        match russh::keys::known_hosts::check_known_hosts_path(&self.host, self.port, key, &path)
        {
            Ok(true) => Ok(true),
            Ok(false) => {
                if self.strict {
                    eprintln!(
                        "kash: rejecting unknown SSH host key ({fingerprint}) — strict host key checking is on"
                    );
                    return Ok(false);
                }
                eprintln!(
                    "kash: warning: trusting new SSH host {}:{} key {fingerprint} (added to {})",
                    self.host,
                    self.port,
                    path.display(),
                );
                if let Err(e) = russh::keys::known_hosts::learn_known_hosts_path(
                    &self.host,
                    self.port,
                    key,
                    &path,
                ) {
                    eprintln!("kash: warning: could not save host key: {e}");
                }
                Ok(true)
            }
            Err(e) => {
                eprintln!("kash: SSH HOST KEY MISMATCH for {}:{}: {e}", self.host, self.port);
                eprintln!("kash: refusing to connect — possible man-in-the-middle attack");
                Ok(false)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Options for dialling an SSH server. Mirrors `SshArgs` (CLI) without
/// clap types so `ssh.rs` stays testable.
pub struct ConnectOptions {
    pub identity: Option<String>,
    pub password: bool,
    /// Reject unknown host keys instead of trusting on first use.
    pub strict_host_key_checking: bool,
    /// Replace the login shell with bash when available (mirrors the reverse
    /// shell's `pty.spawn /bin/bash` upgrade — the transfer/marker protocol
    /// assumes bash). Skipped for Windows targets. Guarded by `command -v`,
    /// so remotes without bash simply keep their login shell.
    pub normalize_shell: bool,
}

/// Command replacing the login shell with bash when present. No-op (and no
/// output) when bash is absent or the shell doesn't understand it.
pub fn bash_normalize_cmd() -> &'static str {
    "command -v bash >/dev/null 2>&1 && exec bash\n"
}

async fn try_agent_auth(
    handle: &mut russh::client::Handle<Client>,
    user: &str,
) -> anyhow::Result<bool> {
    let mut agent = russh::keys::agent::client::AgentClient::connect_env()
        .await
        .map_err(|e| anyhow::anyhow!("ssh-agent connect failed: {e}"))?;
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| anyhow::anyhow!("ssh-agent list failed: {e}"))?;
    for identity in identities {
        let result = match &identity {
            russh::keys::agent::AgentIdentity::PublicKey { key, .. } => handle
                .authenticate_publickey_with(user, key.clone(), None, &mut agent)
                .await
                .map(|a| a.success())
                .unwrap_or(false),
            russh::keys::agent::AgentIdentity::Certificate { certificate, .. } => handle
                .authenticate_certificate_with(user, certificate.clone(), None, &mut agent)
                .await
                .map(|a| a.success())
                .unwrap_or(false),
        };
        if result {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Expand a leading `~/` to the user's home directory. Pure — unit tested.
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from)
        {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

fn stdin_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

fn load_identity_key(path: &str) -> anyhow::Result<Arc<russh::keys::PrivateKey>> {
    let path = expand_tilde(path);
    if !std::path::Path::new(&path).is_file() {
        anyhow::bail!("identity file not found: '{path}'");
    }
    match russh::keys::load_secret_key(&path, None) {
        Ok(key) => Ok(Arc::new(key)),
        Err(first) => {
            // Encrypted key? Prompt for the passphrase when a terminal exists.
            if !stdin_is_terminal() {
                anyhow::bail!(
                    "cannot load identity file '{path}' ({first}); \
                     key may be passphrase-protected — unlock it with ssh-agent or run on a terminal"
                );
            }
            let pass = rpassword::prompt_password(format!("Enter passphrase for '{path}': "))
                .context("passphrase prompt failed")?;
            let key = russh::keys::load_secret_key(&path, Some(&pass))
                .with_context(|| format!("cannot load identity file '{path}'"))?;
            Ok(Arc::new(key))
        }
    }
}

/// Outbound SSH transport: dials, authenticates (identity → agent →
/// password), opens a PTY shell, normalises the login shell to bash, and
/// yields the stream. Reports [`TransportCaps::pty`] — the server already
/// allocated the PTY, so the session loop skips its upgrade step.
pub struct SshTransport {
    /// Remote hostname or IP.
    pub host: String,
    /// SSH port.
    pub port: u16,
    /// Remote user.
    pub user: String,
    /// Auth and shell options (see [`ConnectOptions`]).
    pub opts: ConnectOptions,
}

impl Transport for SshTransport {
    async fn connect(&self) -> anyhow::Result<(BoxReader, BoxWriter, TransportInfo)> {
        let (reader, writer, peer) =
            connect(&self.host, self.port, &self.user, &self.opts).await?;
        let info = TransportInfo::new(TransportKind::Ssh, &peer, TransportCaps::pty());
        Ok((reader, writer, info))
    }
}

/// Dial `host:port`, authenticate, open a PTY shell channel and return the
/// stream split halves plus a display peer string (`user@host:port`).
///
/// Most callers want [`SshTransport`] (the [`Transport`] implementation)
/// instead — this free function is the protocol core it delegates to.
/// The returned stream feeds directly into `session::run_session`.
///
/// The russh channel halves are boxed into [`BoxReader`]/[`BoxWriter`] at the
/// return site so the signature stays decoupled from russh's channel types
/// (which propagate as a 3-type generic parameter through anything that
/// stores them).
pub async fn connect(
    host: &str,
    port: u16,
    user: &str,
    opts: &ConnectOptions,
) -> anyhow::Result<(BoxReader, BoxWriter, String)> {
    let config = Arc::new(russh::client::Config::default());
    let handler = Client {
        host: host.to_string(),
        port,
        strict: opts.strict_host_key_checking,
    };
    let mut handle = russh::client::connect(config, (host, port), handler)
        .await
        .with_context(|| format!("ssh connect to {host}:{port} failed"))?;

    // 1. Explicit identity file.
    let mut authenticated = false;
    if let Some(path) = &opts.identity {
        let key = load_identity_key(path)?;
        // Best-effort RSA hash negotiation; None = non-RSA or no extension.
        let hash_alg = handle.best_supported_rsa_hash().await.unwrap_or(None);
        let auth = handle
            .authenticate_publickey(
                user,
                russh::keys::PrivateKeyWithHashAlg::new(key, hash_alg.flatten()),
            )
            .await
            .with_context(|| format!("ssh publickey auth as '{user}' failed"))?;
        authenticated = auth.success();
        if !authenticated {
            anyhow::bail!("ssh publickey auth as '{user}' rejected (key '{path}')");
        }
    }

    // 2. ssh-agent (only when no explicit identity was given).
    if !authenticated && opts.identity.is_none() {
        match try_agent_auth(&mut handle, user).await {
            Ok(true) => authenticated = true,
            Ok(false) => {}
            Err(e) => {
                // Agent absent is normal (e.g. CI) — fall through to password.
                let _ = e;
            }
        }
    }

    // 3. Password (explicit --password, or fallback when nothing else worked).
    if !authenticated {
        let need_password = opts.password || opts.identity.is_none();
        if need_password {
            if !stdin_is_terminal() {
                anyhow::bail!(
                    "ssh needs a password for '{user}@{host}' but there is no terminal — \
                     use --identity <key> or ssh-agent for background sessions"
                );
            }
            let prompt = format!("{user}@{host}'s password: ");
            let password = rpassword::prompt_password(prompt).context("password prompt failed")?;
            let auth = handle
                .authenticate_password(user, password)
                .await
                .context("ssh password auth failed")?;
            authenticated = auth.success();
            if !authenticated {
                anyhow::bail!("ssh password auth as '{user}' rejected");
            }
        } else {
            anyhow::bail!(
                "ssh auth failed as '{user}' (no agent identity accepted; try --identity or --password)"
            );
        }
    }

    let channel = handle
        .channel_open_session()
        .await
        .context("ssh failed to open session channel")?;

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    channel
        .request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
        .await
        .context("ssh PTY request failed")?;
    channel
        .request_shell(false)
        .await
        .context("ssh shell request failed")?;

    // Keep NATs/firewalls from dropping idle sessions. The task owns the
    // connection handle and exits once the connection is gone; dropping the
    // JoinHandle does not stop it.
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            if handle.send_keepalive(false).await.is_err() || handle.is_closed() {
                break;
            }
        }
    });

    let mut stream = channel.into_stream();
    if opts.normalize_shell {
        // Normalise to bash before the session loop starts. Ordering is
        // guaranteed (same channel), and `probe_identity` tolerates noise.
        stream
            .write_all(bash_normalize_cmd().as_bytes())
            .await
            .context("ssh shell normalisation failed")?;
        stream.flush().await?;
    }
    let (reader, writer) = tokio::io::split(stream);
    Ok((
        Box::new(reader),
        Box::new(writer),
        format!("{user}@{host}:{port}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_at_host() {
        let t = SshTarget::parse("root@10.0.0.5", "kam").unwrap();
        assert_eq!(t.user, "root");
        assert_eq!(t.host, "10.0.0.5");
    }

    #[test]
    fn parse_bare_host_uses_default_user() {
        let t = SshTarget::parse("10.0.0.5", "kam").unwrap();
        assert_eq!(t.user, "kam");
        assert_eq!(t.host, "10.0.0.5");
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(SshTarget::parse("", "kam").is_err());
        assert!(SshTarget::parse("@", "kam").is_err());
        assert!(SshTarget::parse("root@", "kam").is_err());
        assert!(SshTarget::parse("@host", "kam").is_err());
    }

    #[test]
    fn display_includes_port() {
        let t = SshTarget::parse("root@h", "u").unwrap();
        assert_eq!(t.display(2222), "root@h:2222");
    }

    #[test]
    fn bash_normalize_is_guarded_and_single_line() {
        let cmd = bash_normalize_cmd();
        assert!(cmd.ends_with('\n'));
        assert!(!cmd.trim_end().contains('\n'));
        assert!(cmd.contains("exec bash"));
        // Guard: must not kill shells without bash.
        assert!(cmd.contains("command -v bash"));
    }

    #[test]
    fn expand_tilde_with_home() {
        let old = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", "/home/tester");
        }
        assert_eq!(expand_tilde("~/.ssh/id"), "/home/tester/.ssh/id");
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("relative"), "relative");
        unsafe {
            match old {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn expand_tilde_no_home_passthrough() {
        // A bare "~" without trailing slash is a username reference — left alone.
        assert_eq!(expand_tilde("~other/x"), "~other/x");
    }

    #[test]
    fn missing_identity_file_fails_fast() {
        let err = load_identity_key("/nonexistent-path-xyz/kash-test-key")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
    }
}
