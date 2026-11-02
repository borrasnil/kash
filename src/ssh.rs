//! Outbound SSH client (`kash ssh`).
//!
//! Connects to a remote SSH server via `russh`, opens a session channel with
//! a PTY, and hands the resulting byte stream to the shared session loop.
//! Argument and auth semantics mirror `ssh(1)`: `-i/-p/-l/-F/-o` flags, a
//! `~/.ssh/config` parser ([`config`]), and an auth chain of ssh-agent →
//! identity files (explicit `-i`, config `IdentityFile`, then the default
//! key probe) → password when a terminal is available.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use tokio::io::AsyncWriteExt;

use crate::transport::{
    BoxReader, BoxWriter, Transport, TransportCaps, TransportInfo, TransportKind,
};

pub mod config;

// ---------------------------------------------------------------------------
// Target parsing
// ---------------------------------------------------------------------------

/// Parsed `[user@]host`. The user stays optional until resolution — ssh
/// fills it from `-l`, ssh_config `User`, then `$USER`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    pub host: String,
}

impl SshTarget {
    /// Parse `[user@]host`. Empty host is an error.
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            anyhow::bail!("empty SSH target — expected [user@]host");
        }
        let (user, host) = match raw.rsplit_once('@') {
            Some((u, h)) => (Some(u.to_string()), h.to_string()),
            None => (None, raw.to_string()),
        };
        if user.as_ref().is_some_and(String::is_empty) || host.is_empty() {
            anyhow::bail!("invalid SSH target '{raw}' — expected [user@]host");
        }
        Ok(Self { user, host })
    }

    /// Full peer label with explicit port (`root@10.0.0.5:2222`). Used for
    /// the `TransportInfo::peer` display string.
    pub fn display(&self, port: u16, user: &str) -> String {
        format!("{user}@{}:{port}", self.host)
    }
}

/// The user to assume for bare `host` targets: `$USER`/`$USERNAME`, with
/// `"root"` as a last-resort fallback when neither is set.
pub fn default_local_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".to_string())
}

// ---------------------------------------------------------------------------
// Resolution (ssh(1) precedence: CLI > ssh_config > defaults)
// ---------------------------------------------------------------------------

/// ssh(1)'s default `IdentityFile` list (relative to the home directory).
const DEFAULT_IDENTITIES: &[&str] = &[
    ".ssh/id_rsa",
    ".ssh/id_ecdsa",
    ".ssh/id_ecdsa_sk",
    ".ssh/id_ed25519",
    ".ssh/id_ed25519_sk",
    ".ssh/id_mldsa44_ed25519",
];

/// Everything the connect step needs, fully resolved. Mirrors what
/// `ssh -G <target>` would print for the keywords kash supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConnect {
    /// Auth user: CLI `-l`/`user@` prefix > config `User` > `$USER`.
    pub user: String,
    /// Connect host: config `HostName` when set, else the target host.
    pub host: String,
    /// Port: CLI `-p` > config `Port` > 22.
    pub port: u16,
    /// Identity files in try order (after ssh-agent): CLI `-i`, config
    /// `IdentityFile`, then existing default key paths.
    pub identity_files: Vec<String>,
    /// Config file that was consulted, for diagnostics.
    pub config_path: Option<PathBuf>,
}

impl ResolvedConnect {
    /// ssh-G-style diagnostic listing.
    pub fn dump(&self) -> String {
        let mut s = format!(
            "host {}\nuser {}\nport {}\n",
            self.host, self.user, self.port
        );
        match &self.config_path {
            Some(p) => s.push_str(&format!("configfile {}\n", p.display())),
            None => s.push_str("configfile none\n"),
        }
        for f in &self.identity_files {
            s.push_str(&format!("identityfile {f}\n"));
        }
        s
    }
}

/// Resolve target + CLI flags + parsed ssh_config into one [`ResolvedConnect`].
///
/// `home` (when known) scopes default-identity probing to `$HOME`; pass the
/// real home from the CLI caller, tests pass a fixture dir.
pub fn resolve_connect(
    target: &SshTarget,
    cli_user: Option<&str>,
    cli_port: Option<u16>,
    cli_identity: &[String],
    cfg: &config::SshConfig,
    home: Option<&Path>,
) -> ResolvedConnect {
    // User: CLI (-l or user@ prefix) beats config User, beats $USER.
    let user = cli_user
        .map(str::to_string)
        .or_else(|| target.user.clone())
        .or_else(|| cfg.user.clone())
        .unwrap_or_else(default_local_user);

    // Host: config HostName (must be non-empty) overrides the alias.
    let host = cfg
        .host_name
        .clone()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| target.host.clone());

    // Port: explicit CLI value beats config; 0 args are ignored in both.
    let port = match cli_port {
        Some(p) if p > 0 => p,
        _ => {
            let cfg_port = cfg.port.filter(|p| *p > 0);
            cfg_port.unwrap_or(22)
        }
    };

    // Identity files: CLI -i, then config IdentityFile, then defaults that
    // exist on disk (ssh warns rather than errors on missing -i paths).
    let mut identity_files: Vec<String> = cli_identity
        .iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| expand_tilde(p))
        .collect();
    for f in &cfg.identity_files {
        let expanded = expand_tilde(f);
        if !identity_files.contains(&expanded) {
            identity_files.push(expand_tilde(f));
        }
    }
    for f in default_identity_paths(home) {
        if !identity_files.contains(&f) {
            identity_files.push(f);
        }
    }

    ResolvedConnect {
        user,
        host,
        port,
        identity_files,
        config_path: None,
    }
}

/// Default identity files that exist under `home`, in ssh(1) order.
fn default_identity_paths(home: Option<&Path>) -> Vec<String> {
    let Some(home) = home else {
        return Vec::new();
    };
    DEFAULT_IDENTITIES
        .iter()
        .map(|rel| {
            #[cfg(windows)]
            let lower_rel = rel.replace('/', "\\");
            #[cfg(windows)]
            let rel: &str = &lower_rel;
            home.join(rel).to_string_lossy().into_owned()
        })
        .filter(|p| Path::new(p).is_file())
        .collect()
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

/// Path of the user's `~/.ssh/known_hosts` (or `None` without a home
/// directory, in which case host-key checking is skipped).
fn known_hosts_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| std::path::PathBuf::from(h).join(".ssh").join("known_hosts"))
}

impl russh::client::Handler for Client {
    type Error = russh::Error;

    /// [`russh::client::Handler::check_server_key`] — the TOFU / strict /
    /// mismatch logic described on the struct lives here. Certificates are
    /// accepted with a warning (certificate-based deployments instead of
    /// plain host keys are rare).
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

/// Options for dialling an SSH server. Mirrors `SshArgs` after resolution
/// (no clap types) so `ssh.rs` stays testable.
pub struct ConnectOptions {
    /// Identity files to try in order (after ssh-agent), tilde-expanded.
    pub identity_files: Vec<String>,
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

/// Try every identity held by the user's ssh-agent (public keys then
/// certificates). Returns `Ok(true)` on first success, `Ok(false)` when the
/// agent has identities but none were accepted; connection agent errors
/// surface as `Err`.
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

/// Whether an interactive password/passphrase prompt is possible at all —
/// false for background daemons and piped stdin.
fn stdin_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Load a private key from `path` (with `~/` expansion). Passphrase-protected
/// keys prompt on a terminal via `rpassword` when `allow_prompt`; headless
/// callers should pass keys pre-unlocked by an ssh-agent or skip.
fn load_identity_key(path: &str, allow_prompt: bool) -> anyhow::Result<Arc<russh::keys::PrivateKey>> {
    let path = expand_tilde(path);
    if !std::path::Path::new(&path).is_file() {
        anyhow::bail!("identity file not found: '{path}'");
    }
    match russh::keys::load_secret_key(&path, None) {
        Ok(key) => Ok(Arc::new(key)),
        Err(first) => {
            // Encrypted key? Prompt for the passphrase when allowed.
            if !allow_prompt {
                anyhow::bail!(
                    "key may be passphrase-protected ({first}); \
                     unlock it with ssh-agent or run on a terminal"
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

    // Auth follows ssh(1): agent identities first, then identity files in
    // try order, then password when a terminal exists.
    let mut authenticated = false;
    if !authenticated {
        match try_agent_auth(&mut handle, user).await {
            Ok(true) => authenticated = true,
            Ok(false) => {}
            Err(e) => {
                // Agent absent is normal (e.g. CI) — fall through to keys.
                let _ = e;
            }
        }
    }

    let mut last_key_error: Option<String> = None;
    for path in &opts.identity_files {
        let key = match load_identity_key(path, stdin_is_terminal()) {
            Ok(key) => key,
            Err(e) => {
                eprintln!("kash: skipping identity file '{path}': {e}");
                continue;
            }
        };
        // Best-effort RSA hash negotiation; None = non-RSA or no extension.
        let hash_alg = handle.best_supported_rsa_hash().await.unwrap_or(None);
        let auth = handle
            .authenticate_publickey(
                user,
                russh::keys::PrivateKeyWithHashAlg::new(key, hash_alg.flatten()),
            )
            .await
            .with_context(|| format!("ssh publickey auth as '{user}' failed"))?;
        if auth.success() {
            authenticated = true;
            break;
        }
        last_key_error = Some(format!("key '{path}' rejected"));
    }

    // 3. Password — ssh(1) prompts by default for interactive sessions.
    if !authenticated {
        if stdin_is_terminal() || opts.password {
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
            let tried = opts.identity_files.len();
            let detail = last_key_error.unwrap_or_else(|| "no usable identity".into());
            anyhow::bail!(
                "ssh auth failed as '{user}@{host}' — tried ssh-agent and {tried} identity \
                 file(s) ({detail}); run on a terminal for password auth or pass -i <key>"
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
        let t = SshTarget::parse("root@10.0.0.5").unwrap();
        assert_eq!(t.user.as_deref(), Some("root"));
        assert_eq!(t.host, "10.0.0.5");
    }

    #[test]
    fn parse_bare_host_keeps_user_optional() {
        let t = SshTarget::parse("10.0.0.5").unwrap();
        assert_eq!(t.user, None);
        assert_eq!(t.host, "10.0.0.5");
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(SshTarget::parse("").is_err());
        assert!(SshTarget::parse("@").is_err());
        assert!(SshTarget::parse("root@").is_err());
        assert!(SshTarget::parse("@host").is_err());
    }

    #[test]
    fn display_includes_port() {
        let t = SshTarget::parse("root@h").unwrap();
        assert_eq!(t.display(2222, "root"), "root@h:2222");
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
        let err = load_identity_key("/nonexistent-path-xyz/kash-test-key", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
    }

    // -- resolution ----------------------------------------------------------

    use std::path::Path as StdPath;
    use std::path::PathBuf as StdPathBuf;

    fn fixture_home(tag: &str) -> StdPathBuf {
        let dir = std::env::temp_dir().join(format!("kash-ssh-home-{}-{tag}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join(".ssh"));
        dir
    }

    fn res(
        raw: &str,
        login: Option<&str>,
        port: Option<u16>,
        ids: &[&str],
        cfg: config::SshConfig,
        home: Option<&StdPath>,
    ) -> ResolvedConnect {
        let target = SshTarget::parse(raw).unwrap();
        let owned: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        resolve_connect(&target, login, port, &owned, &cfg, home)
    }

    #[test]
    fn resolve_cli_beats_config_beats_defaults() {
        let cfg = config::SshConfig {
            matched: "kali".into(),
            host_name: Some("10.0.0.5".into()),
            user: Some("parallels".into()),
            port: Some(2222),
            identity_files: Vec::new(),
        };
        // Bare target: config values apply.
        let r = res("kali", None, None, &[], cfg.clone(), None);
        assert_eq!(r.user, "parallels");
        assert_eq!(r.host, "10.0.0.5");
        assert_eq!(r.port, 2222);
        // CLI beats config.
        let r2 = res("parallels@kali", Some("root"), Some(22), &[], cfg, None);
        assert_eq!(r2.user, "root");
        assert_eq!(r2.port, 22);
        // user@ prefix applied when no -l.
        let r3 = res("parallels@kali", None, None, &[], config::SshConfig::default(), None);
        assert_eq!(r3.user, "parallels");
    }

    #[test]
    fn identity_order_cli_config_defaults() {
        let home = fixture_home("order");
        // Only one default key exists → it must be the last candidate.
        std::fs::write(home.join(".ssh/id_ed25519"), b"x").unwrap();
        let cfg = config::SshConfig {
            identity_files: vec!["~/cfg_key".into()],
            ..Default::default()
        };
        let r = res(
            "kali",
            None,
            None,
            &["~/cli_first"],
            cfg,
            Some(home.as_path()),
        );
        assert_eq!(
            r.identity_files,
            vec![
                expand_tilde("~/cli_first"),
                expand_tilde("~/cfg_key"),
                home.join(".ssh/id_ed25519").to_string_lossy().into_owned(),
            ]
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn default_probe_skips_absent_keys() {
        let home = fixture_home("skip-absent");
        let r = res("kali", None, None, &[], config::SshConfig::default(), Some(home.as_path()));
        assert!(r.identity_files.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn no_home_no_defaults() {
        let r = res("kali", None, None, &[], config::SshConfig::default(), None);
        assert!(r.identity_files.is_empty());
    }

    #[test]
    fn config_hostname_overrides_alias() {
        let cfg = config::SshConfig {
            host_name: Some("real-host.internal".into()),
            ..Default::default()
        };
        let r = res("kali", None, None, &[], cfg, None);
        assert_eq!(r.host, "real-host.internal");
    }

    #[test]
    fn dump_lists_resolved() {
        let r = ResolvedConnect {
            user: "parallels".into(),
            host: "kali".into(),
            port: 22,
            identity_files: vec!["~/.ssh/id_ed25519".into()],
            config_path: Some(StdPathBuf::from("/Users/kam/.ssh/config")),
        };
        let d = r.dump();
        assert!(d.contains("host kali") && d.contains("user parallels"));
        assert!(d.contains("identityfile ~/.ssh/id_ed25519"));
    }
}
