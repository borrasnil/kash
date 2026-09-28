//! `kash listen` — start a listener and wait for an incoming reverse shell.

use std::io::Write as _;
use std::sync::LazyLock;

use anyhow::Context;
use rand::seq::SliceRandom;

use crate::agent;
use crate::cli::ListenArgs;
use crate::listen::listen;
use crate::obfuscation::create_strategy;
use crate::prompt;
use crate::session::run_session;

/// Popular, easy-to-type animal names used for auto-generated session IDs.
static ANIMAL_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    vec![
        "monkey", "tiger", "lion", "bear", "wolf", "fox", "deer", "eagle",
        "hawk", "crow", "owl", "snake", "zebra", "horse", "goat", "sheep",
        "camel", "moose", "elk", "bison", "rat", "rabbit", "otter", "beaver",
        "badger", "whale", "shark", "dolphin", "turtle", "frog", "lizard",
        "hyena", "leopard", "panda", "koala", "giraffe", "hippo", "crab",
        "squid", "penguin",
    ]
});

pub async fn cmd_listen(args: ListenArgs) -> anyhow::Result<()> {
    let session_id = args
        .session
        .clone()
        .unwrap_or_else(generate_session_id);

    // Daemon mode: spawn a headless child, print the session info, and return.
    // The child handles the TCP accept and runs the session without a terminal.
    if args.daemon {
        return spawn_daemon(&args, &session_id);
    }

    if !args.headless {
        print!(
            "{}",
            prompt::startup_banner(
                &args.listen,
                args.port,
                args.obfuscation_level(),
                args.shell_type(),
                &session_id,
            )
        );
        let _ = std::io::stdout().flush();
    }

    let (stream, peer_addr) = listen(&args.listen, args.port)
        .await
        .with_context(|| format!("failed to listen on {}:{}", args.listen, args.port))?;

    let engine = create_strategy(args.obfuscation_level(), args.shell_type());

    run_session(
        stream,
        peer_addr,
        &*engine,
        args.obfuscation_level(),
        args.shell_type(),
        &session_id,
        args.headless,
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// daemon spawner
// ---------------------------------------------------------------------------

/// Spawn a background (headless) worker with the same listener config, then
/// return immediately — the calling process exits and the terminal is freed.
#[cfg(unix)]
fn spawn_daemon(args: &ListenArgs, session_id: &str) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    print!(
        "{}",
        prompt::startup_banner_daemon(
            &args.listen,
            args.port,
            args.obfuscation_level(),
            args.shell_type(),
            session_id,
        )
    );
    let _ = std::io::stdout().flush();

    let exe = std::env::current_exe().context("cannot resolve current executable path")?;
    let null = std::fs::File::open("/dev/null").context("cannot open /dev/null")?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("listen")
        .arg(args.port.to_string())
        .arg("--listen").arg(&args.listen)
        .arg("--obfuscation").arg(args.obfuscation.to_string())
        .arg("--shell").arg(args.shell.to_string())
        .arg("--session").arg(session_id)
        .arg("--headless")
        .stdin(null.try_clone()?)
        .stdout(null.try_clone()?)
        .stderr(null);

    // Detach from the controlling terminal so SIGHUP on terminal close
    // doesn't kill the background session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    cmd.spawn().context("failed to spawn daemon process")?;
    Ok(())
}

#[cfg(not(unix))]
fn spawn_daemon(_args: &ListenArgs, _session_id: &str) -> anyhow::Result<()> {
    anyhow::bail!("--daemon (-d) is not supported on this platform (Unix only)")
}

// ---------------------------------------------------------------------------
// Session ID generation
// ---------------------------------------------------------------------------

/// True if `id` is already assigned to a currently-active session (same scan as `ps`).
fn session_id_in_use(id: &str) -> bool {
    agent::list_sessions().iter().any(|s| s.id == id)
}

fn generate_session_id() -> String {
    let mut rng = rand::thread_rng();
    let mut candidates: Vec<&str> = ANIMAL_NAMES.clone();
    candidates.shuffle(&mut rng);
    if let Some(name) = candidates.into_iter().find(|n| !session_id_in_use(n)) {
        return name.to_string();
    }
    // All animal names are in use — append a numeric suffix.
    let base = ANIMAL_NAMES.first().copied().unwrap_or("session");
    (2..)
        .map(|i| format!("{base}{i}"))
        .find(|candidate| !session_id_in_use(candidate))
        .unwrap_or_else(|| format!("{base}-{}", crate::util::generate_nonce()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_an_animal() {
        let id = generate_session_id();
        let base = id.trim_end_matches(|c: char| c.is_ascii_digit());
        assert!(ANIMAL_NAMES.contains(&base), "unexpected session id: {id}");
        assert!(!session_id_in_use(&id));
    }

    #[test]
    fn session_id_in_use_detects_active_session() {
        assert!(!session_id_in_use("__test_in_use__"));
        std::fs::write(agent::socket_path("__test_in_use__"), b"").unwrap();
        assert!(session_id_in_use("__test_in_use__"));
        let _ = std::fs::remove_file(agent::socket_path("__test_in_use__"));
    }
}
