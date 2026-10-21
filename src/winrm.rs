//! Windows Remote Management transport (`kash winrm`).
//!
//! Connects over HTTP(S) SOAP via the [`winrm-rs`](https://docs.rs/winrm-rs)
//! client (NTLMv2/Basic, system TLS roots), opens one persistent shell, and
//! starts a long-running interpreter (`powershell -Command -` or `cmd /q`)
//! as an interactive REPL. A background task pumps that REPL into a
//! [`tokio::io::duplex`] byte stream, so the shared session loop, agent
//! IPC, transfers, and modules work unchanged — with
//! [`TransportCaps::no_pty`] (handler/line-editor mode, never raw).
//!
//! Auth chain: `--password` prompt, else `WINRM_PASSWORD` env (the only
//! option for `-d` background sessions), else interactive prompt. Password
//! prompts require a terminal and fail loudly otherwise.
//!
//! `DOMAIN\user` logins are split into domain + user for NTLM; UPNs
//! (`user@domain`) pass through whole.

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::transport::{
    BoxReader, BoxWriter, RemoteShell, Transport, TransportCaps, TransportInfo, TransportKind,
};

// ---------------------------------------------------------------------------
// Target
// ---------------------------------------------------------------------------

/// Default WinRM HTTP port.
pub const DEFAULT_HTTP_PORT: u16 = 5985;

/// Default WinRM HTTPS port.
pub const DEFAULT_HTTPS_PORT: u16 = 5986;

/// Parsed `[user@]host`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinRmTarget {
    /// Remote user (defaults to the local user when omitted).
    pub user: String,
    /// Hostname or IP.
    pub host: String,
}

impl WinRmTarget {
    /// Parse `[user@]host`. Empty user/host is an error.
    ///
    /// ```
    /// use kash::winrm::WinRmTarget;
    ///
    /// let t = WinRmTarget::parse("Admin@10.0.0.5", "kam").unwrap();
    /// assert_eq!(t.user, "Admin");
    /// assert_eq!(t.host, "10.0.0.5");
    /// let t = WinRmTarget::parse("10.0.0.5", "kam").unwrap();
    /// assert_eq!(t.user, "kam");
    /// ```
    pub fn parse(raw: &str, default_user: &str) -> anyhow::Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            anyhow::bail!("empty WinRM target — expected [user@]host");
        }
        let (user, host) = match raw.rsplit_once('@') {
            Some((u, h)) => (u.to_string(), h.to_string()),
            None => (default_user.to_string(), raw.to_string()),
        };
        if user.is_empty() || host.is_empty() {
            anyhow::bail!("invalid WinRM target '{raw}' — expected [user@]host");
        }
        Ok(Self { user, host })
    }
}

// ---------------------------------------------------------------------------
// Auth scheme
// ---------------------------------------------------------------------------

/// WinRM auth scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinRmAuth {
    /// NTLMv2 challenge-response (default; works over plain HTTP).
    Ntlm,
    /// Username/password Basic auth. Requires `--ssl` in production —
    /// without TLS the password travels in cleartext.
    Basic,
}

impl WinRmAuth {
    /// Parse `basic` | `ntlm`.
    ///
    /// ```
    /// use kash::winrm::WinRmAuth;
    /// assert_eq!(WinRmAuth::parse("NTLM").unwrap(), WinRmAuth::Ntlm);
    /// assert!(WinRmAuth::parse("kerberos").is_err());
    /// ```
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.to_lowercase().as_str() {
            "ntlm" => Ok(Self::Ntlm),
            "basic" => Ok(Self::Basic),
            _ => anyhow::bail!("invalid WinRM auth '{s}': expected basic | ntlm"),
        }
    }
}

/// Clap wrapper so `--auth` parses via [`WinRmAuth::parse`].
#[derive(Debug, Clone)]
pub struct WinRmAuthArg(pub WinRmAuth);

impl std::str::FromStr for WinRmAuthArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        WinRmAuth::parse(s)
            .map(Self)
            .map_err(|e| format!("{e:#}"))
    }
}

impl std::fmt::Display for WinRmAuthArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            WinRmAuth::Ntlm => write!(f, "ntlm"),
            WinRmAuth::Basic => write!(f, "basic"),
        }
    }
}

// ---------------------------------------------------------------------------
// Remote shell
// ---------------------------------------------------------------------------

/// Shell opened inside the WinRM session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinRmShell {
    /// `powershell -NoProfile`; file transfers reuse the existing
    /// PowerShell base64 backend.
    Powershell,
    /// `cmd.exe`; narrower quoting rules, same marker protocol.
    Cmd,
}

impl WinRmShell {
    /// Parse `powershell` | `cmd` (also accepts `ps`).
    ///
    /// ```
    /// use kash::winrm::WinRmShell;
    /// assert_eq!(WinRmShell::parse("powershell").unwrap(), WinRmShell::Powershell);
    /// assert_eq!(WinRmShell::parse("cmd").unwrap(), WinRmShell::Cmd);
    /// assert!(WinRmShell::parse("bash").is_err());
    /// ```
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.to_lowercase().as_str() {
            "powershell" | "ps" => Ok(Self::Powershell),
            "cmd" => Ok(Self::Cmd),
            _ => anyhow::bail!("invalid WinRM shell '{s}': expected powershell | cmd"),
        }
    }

    /// Interpreter launched on the target.
    pub fn interpreter(self) -> &'static str {
        match self {
            Self::Powershell => "powershell",
            Self::Cmd => "cmd",
        }
    }
}

/// Clap wrapper so `--shell` parses via [`WinRmShell::parse`].
#[derive(Debug, Clone)]
pub struct WinRmShellArg(pub WinRmShell);

impl std::str::FromStr for WinRmShellArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        WinRmShell::parse(s)
            .map(Self)
            .map_err(|e| format!("{e:#}"))
    }
}

impl std::fmt::Display for WinRmShellArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            WinRmShell::Powershell => write!(f, "powershell"),
            WinRmShell::Cmd => write!(f, "cmd"),
        }
    }
}

// ---------------------------------------------------------------------------
// Endpoint
// ---------------------------------------------------------------------------

/// Effective port: explicit `--port` wins, otherwise 5986 with `--ssl`,
/// 5985 without.
///
/// ```
/// use kash::winrm::resolve_port;
/// assert_eq!(resolve_port(None, false), 5985);
/// assert_eq!(resolve_port(None, true), 5986);
/// assert_eq!(resolve_port(Some(55985), true), 55985);
/// ```
pub fn resolve_port(explicit: Option<u16>, ssl: bool) -> u16 {
    explicit.unwrap_or(if ssl {
        DEFAULT_HTTPS_PORT
    } else {
        DEFAULT_HTTP_PORT
    })
}

/// WS-Management endpoint URL for the SOAP connector.
///
/// ```
/// use kash::winrm::endpoint;
/// assert_eq!(endpoint(false, "10.0.0.5", 5985), "http://10.0.0.5:5985/wsman");
/// assert_eq!(endpoint(true, "host", 5986), "https://host:5986/wsman");
/// ```
pub fn endpoint(ssl: bool, host: &str, port: u16) -> String {
    let scheme = if ssl { "https" } else { "http" };
    format!("{scheme}://{host}:{port}/wsman")
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// WinRM transport: everything needed to dial, authenticate, and open a
/// REPL shell. Consumed by [`Transport::connect`].
pub struct WinRmTransport {
    /// Remote hostname or IP.
    pub host: String,
    /// WinRM port (see [`resolve_port`]).
    pub port: u16,
    /// HTTPS transport.
    pub ssl: bool,
    /// Skip TLS verification (self-signed certs).
    pub insecure: bool,
    /// Remote user (`DOMAIN\user` supported, split for NTLM).
    pub user: String,
    /// Resolved password (see [`resolve_password`]).
    pub password: String,
    /// Auth scheme for the SOAP session.
    pub auth: WinRmAuth,
    /// Interpreter started as the REPL.
    pub shell: WinRmShell,
}

/// Split `DOMAIN\user` into `(domain, user)` for NTLM; anything else
/// passes through with an empty domain (UPNs stay whole).
///
/// ```
/// use kash::winrm::split_domain;
/// assert_eq!(split_domain("CORP\\Admin"), ("CORP".to_string(), "Admin".to_string()));
/// assert_eq!(split_domain("admin@corp.example"), ("".to_string(), "admin@corp.example".to_string()));
/// assert_eq!(split_domain("Admin"), ("".to_string(), "Admin".to_string()));
/// ```
pub fn split_domain(user: &str) -> (String, String) {
    match user.rsplit_once('\\') {
        Some((d, u)) => (d.to_string(), u.to_string()),
        None => (String::new(), user.to_string()),
    }
}

/// Resolve the WinRM password: `--password` forces an interactive prompt,
/// otherwise `WINRM_PASSWORD` (automation and `-d` background sessions),
/// otherwise prompt. Prompts need a terminal and bail loudly without one.
///
/// ```
/// // `std::env` mutation is `unsafe` under edition 2024; doctests run in
/// // threads of one process, so scope the mutation tightly.
/// # unsafe {
/// std::env::set_var("WINRM_PASSWORD", "s3cret");
/// # }
/// let pw = kash::winrm::resolve_password(false).unwrap();
/// assert_eq!(pw, "s3cret");
/// # unsafe {
/// std::env::remove_var("WINRM_PASSWORD");
/// # }
/// ```
pub fn resolve_password(password_flag: bool) -> anyhow::Result<String> {
    if !password_flag {
        if let Ok(pw) = std::env::var("WINRM_PASSWORD") {
            if !pw.is_empty() {
                return Ok(pw);
            }
        }
    }
    if !stdin_is_terminal() {
        anyhow::bail!(
            "winrm needs a password but there is no terminal — \
             set WINRM_PASSWORD or run on a terminal (see --password)"
        );
    }
    rpassword::prompt_password("WinRM password: ").context("password prompt failed")
}

/// Returns true when stdin is a terminal (password prompts usable).
fn stdin_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

impl WinRmTransport {
    /// Peer label for the session loop (`user@host:port (winrm)`).
    pub fn peer_label(user: &str, host: &str, port: u16) -> String {
        format!("{user}@{host}:{port} (winrm)")
    }

    /// [`TransportInfo`] for WinRM sessions: no PTY (handler mode) plus
    /// the remote command dialect for marker framing.
    pub fn info(user: &str, host: &str, port: u16, shell: WinRmShell) -> TransportInfo {
        let dialect = match shell {
            WinRmShell::Powershell => RemoteShell::Powershell,
            WinRmShell::Cmd => RemoteShell::Cmd,
        };
        TransportInfo::new(
            TransportKind::Winrm,
            &Self::peer_label(user, host, port),
            TransportCaps::no_pty(),
        )
        .with_shell(dialect)
    }

    /// REPL interpreter + args started as the single long-running command.
    fn repl_command(shell: WinRmShell) -> (&'static str, &'static [&'static str]) {
        match shell {
            // `-` reads commands from stdin; `-NonInteractive` suppresses prompts.
            WinRmShell::Powershell => (
                "powershell",
                &["-NoProfile", "-NonInteractive", "-Command", "-"],
            ),
            // `/q` turns echo off so typed input isn't reflected.
            WinRmShell::Cmd => ("cmd", &["/q"]),
        }
    }
}

impl Transport for WinRmTransport {
    fn connect(
        &self,
    ) -> impl std::future::Future<Output = anyhow::Result<(BoxReader, BoxWriter, TransportInfo)>> + Send
    {
        // The REPL `Shell` borrows its `WinrmClient`, so both are built
        // inside the pump task; a oneshot carries setup errors back here.
        // Without this, moving the client into the task while borrowed
        // fails to compile (E0597/E0505).
        let params = ReplParams {
            host: self.host.clone(),
            port: self.port,
            ssl: self.ssl,
            insecure: self.insecure,
            user: self.user.clone(),
            password: self.password.clone(),
            auth: self.auth,
            shell: self.shell,
        };
        let info = Self::info(&self.user, &self.host, self.port, self.shell);
        async move {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let (session_end, task_end) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                serve_repl(params, task_end, ready_tx).await;
            });
            ready_rx
                .await
                .map_err(|_| anyhow::anyhow!("winrm pump task died during setup"))?
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let (reader, writer) = tokio::io::split(session_end);
            Ok((Box::new(reader) as BoxReader, Box::new(writer) as BoxWriter, info))
        }
    }
}

/// Owned inputs for [`serve_repl`]: everything the pump task needs to dial
/// and open the REPL, safe to move across the spawn boundary.
struct ReplParams {
    host: String,
    port: u16,
    ssl: bool,
    insecure: bool,
    user: String,
    password: String,
    auth: WinRmAuth,
    shell: WinRmShell,
}

/// Build the client, open the shell, start the REPL, report readiness,
/// then pump until either side closes.
///
/// Setup and pump share one scope on purpose: the REPL `Shell` borrows its
/// `WinrmClient`, so neither may be moved out while the other lives
/// (returning the pair fails with E0505/E0515). The pump takes the shell
/// by value while the client outlives the call — the one arrangement the
/// borrow checker accepts.
async fn serve_repl(
    params: ReplParams,
    stream: tokio::io::DuplexStream,
    ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
) {
    macro_rules! fail {
        ($e:expr) => {{
            let _ = ready.send(Err($e));
            return;
        }};
    }

    let config = winrm_rs::WinrmConfig {
        port: params.port,
        use_tls: params.ssl,
        accept_invalid_certs: params.insecure,
        auth_method: match params.auth {
            WinRmAuth::Ntlm => winrm_rs::AuthMethod::Ntlm,
            WinRmAuth::Basic => winrm_rs::AuthMethod::Basic,
        },
        // Short server-side poll windows keep the interactive
        // stream responsive; the reader loop simply re-polls.
        operation_timeout_secs: 30,
        ..Default::default()
    };
    let (domain, user) = split_domain(&params.user);
    let creds = winrm_rs::WinrmCredentials::new(user, params.password.clone(), domain);
    let client = match winrm_rs::WinrmClient::new(config, creds) {
        Ok(c) => c,
        Err(e) => fail!(anyhow::anyhow!("winrm client setup failed: {e}")),
    };
    let shell = match client.open_shell(&params.host).await {
        Ok(s) => s,
        Err(e) => fail!(anyhow::anyhow!("winrm open shell on {} failed: {e}", params.host)),
    };
    let (cmd, cmd_args) = WinRmTransport::repl_command(params.shell);
    let cmd_id = match shell.start_command(cmd, cmd_args).await {
        Ok(id) => id,
        Err(e) => fail!(anyhow::anyhow!("winrm start {cmd} failed: {e}")),
    };
    let _ = ready.send(Ok(()));
    pump_repl(shell, cmd_id, stream).await;
    // `shell` drops here — winrm-rs closes it best-effort.
    drop(client);
}

/// Largest single `Send` chunk: WinRS command lines cap near 8 KB, and
/// staying far below keeps base64 transfer lines intact server-side.
const SEND_CHUNK: usize = 4096;

/// Pump loop: REPL stdin from `stream`, REPL stdout/stderr into `stream`.
///
/// * Inbound `[0x03]` alone is CTRL+C → `signal_ctrl_c`, not typed input.
/// * `done` (REPL exited) or any transport error ends the stream (EOF),
///   which the session loop treats as a closed connection.
async fn pump_repl(
    shell: winrm_rs::Shell<'_>,
    cmd_id: String,
    mut stream: tokio::io::DuplexStream,
) {
    let mut inbound = vec![0u8; 8192];
    loop {
        tokio::select! {
            biased;

            n = stream.read(&mut inbound) => {
                let n = n.unwrap_or(0);
                if n == 0 {
                    break;
                }
                if inbound[..n] == [0x03] {
                    let _ = shell.signal_ctrl_c(&cmd_id).await;
                    continue;
                }
                for chunk in inbound[..n].chunks(SEND_CHUNK) {
                    if shell.send_input(&cmd_id, chunk, false).await.is_err() {
                        // Break to the single exit point so shell.close() always runs.
                        break;
                    }
                }
            }

            out = shell.receive_next(&cmd_id) => {
                match out {
                    Ok(chunk) => {
                        let mut buf = Vec::with_capacity(chunk.stdout.len() + chunk.stderr.len());
                        buf.extend_from_slice(&chunk.stdout);
                        buf.extend_from_slice(&chunk.stderr);
                        if !buf.is_empty()
                            && stream.write_all(&buf).await.is_err() {
                                return;
                            }
                        if chunk.done {
                            let _ = stream.flush().await;
                            let _ = stream.shutdown().await;
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("kash: winrm: receive error: {e}");
                        break;
                    }
                }
            }
        }
    }
    let _ = shell.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parsing() {
        assert!(WinRmTarget::parse("", "u").is_err());
        assert!(WinRmTarget::parse("Admin@", "u").is_err());
        assert!(WinRmTarget::parse("@h", "u").is_err());
        let t = WinRmTarget::parse("Admin@h", "u").unwrap();
        assert_eq!((t.user, t.host), ("Admin".to_string(), "h".to_string()));
    }

    #[test]
    fn auth_roundtrip() {
        assert_eq!(WinRmAuthArg(WinRmAuth::Ntlm).to_string(), "ntlm");
        assert!("basic".parse::<WinRmAuthArg>().is_ok());
        assert!("negotiate".parse::<WinRmAuthArg>().is_err());
    }

    #[test]
    fn shell_roundtrip() {
        assert_eq!(WinRmShell::Powershell.interpreter(), "powershell");
        assert_eq!(WinRmShell::Cmd.interpreter(), "cmd");
        assert_eq!(WinRmShellArg(WinRmShell::Cmd).to_string(), "cmd");
        assert!("ps".parse::<WinRmShellArg>().is_ok());
        assert!("sh".parse::<WinRmShellArg>().is_err());
    }

    #[test]
    fn info_has_no_pty_and_powershell_dialect() {
        let info = WinRmTransport::info("Admin", "h", 5985, WinRmShell::Powershell);
        assert_eq!(info.kind, TransportKind::Winrm);
        assert!(!info.caps.has_pty);
        assert!(info.peer.contains("winrm"));
        assert_eq!(info.shell, crate::transport::RemoteShell::Powershell);
        let info = WinRmTransport::info("Admin", "h", 5985, WinRmShell::Cmd);
        assert_eq!(info.shell, crate::transport::RemoteShell::Cmd);
    }

    #[test]
    fn repl_commands() {
        let (cmd, args) = WinRmTransport::repl_command(WinRmShell::Powershell);
        assert_eq!(cmd, "powershell");
        assert!(args.contains(&"-Command"));
        let (cmd, args) = WinRmTransport::repl_command(WinRmShell::Cmd);
        assert_eq!(cmd, "cmd");
        assert!(args.contains(&"/q"));
    }

    #[test]
    fn domain_split() {
        assert_eq!(
            split_domain("CORP\\Admin"),
            ("CORP".to_string(), "Admin".to_string())
        );
        assert_eq!(split_domain("Admin"), ("".to_string(), "Admin".to_string()));
    }

    #[test]
    fn password_prefers_env() {
        let old = std::env::var_os("WINRM_PASSWORD");
        unsafe {
            std::env::set_var("WINRM_PASSWORD", "env-secret");
        }
        assert_eq!(resolve_password(false).unwrap(), "env-secret");
        unsafe {
            match old {
                Some(v) => std::env::set_var("WINRM_PASSWORD", v),
                None => std::env::remove_var("WINRM_PASSWORD"),
            }
        }
    }
}
