//! Session management: the core event loop for an interactive shell session.
//!
//! Handles the bidirectional data flow between the local terminal and the
//! remote shell, including:
//! * Raw PTY passthrough and handler-mode line editing
//! * Agent command injection via IPC
//! * Interactive client attach/detach
//! * File transfer coordination

mod display;
pub mod frame;
pub mod id;
mod meta;
mod state;

use std::io::{self, Write};
use std::time::Duration;

use crossterm::{
    cursor,
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    queue,
    terminal::{self, Clear, ClearType},
};
use futures_util::StreamExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::agent::{AgentCommand, AgentResponse};
use crate::cli::ShellType;
use crate::terminal::line_editor::{LineAction, LineEditor};
use crate::output::{clean_output, display_output};
use crate::prompt;
use crate::terminal::RawModeGuard;
use crate::transport::{BoxReader, BoxWriter, RemoteShell, TransportCaps, TransportInfo};
use crate::util::{key_to_bytes, raw_normalize};

use crate::{agent, transfer};

use display::{clear_input, redraw_input};
use frame::{agent_command, framed_output, probe_command};
use meta::{execute_raw_meta, last_line, parse_meta, parse_meta_raw, strip_done_marker, strip_start_marker};
use state::{CleanupGuard, DisplayState, LiveMeta, SessionState};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Takes [`BoxReader`]/[`BoxWriter`] rather than generic stream types so the
/// bulk of the loop compiles once regardless of transport (reverse, SSH,
/// WinRM, …). Each call site boxes its concrete streams; the trait-object
/// indirection costs one virtual call per read/write and shrinks the
/// binary by tens of KiB per added transport.
pub async fn run_session(
    mut reader: BoxReader,
    mut writer: BoxWriter,
    shell_type: ShellType,
    session_id: &str,
    headless: bool,
    transport: TransportInfo,
) -> Result<(), anyhow::Error> {
    // Headless mode (daemon child): no controlling terminal — skip raw mode.
    let mut raw_guard: Option<RawModeGuard> = if headless {
        None
    } else {
        Some(RawModeGuard::enable()?)
    };
    let mut stdout = io::stdout();

    write!(stdout, "\r\n{}\r\n\r\n", prompt::banner_connected(&transport.peer, session_id))?;
    stdout.flush()?;

    // Probe remote identity.
    write!(stdout, "\x1b[2m  probing identity...\x1b[0m")?;
    stdout.flush()?;
    let (user, host) = probe_identity(&mut reader, &mut writer, transport.shell).await;
    queue!(stdout, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    stdout.flush()?;

    let mut meta =
        LiveMeta::new(session_id, &transport, &user, &host, shell_type);
    meta.persist();
    let _cleanup = CleanupGuard::new(session_id);

    let (agent_tx, mut agent_rx) = mpsc::channel::<AgentCommand>(16);
    let (attach_tx, mut attach_rx) = mpsc::channel::<tokio::net::UnixStream>(4);
    if let Err(e) = agent::serve(session_id, agent_tx, attach_tx) {
        write!(stdout, "\x1b[1;33m[!]\x1b[0m agent socket unavailable: {e}\r\n")?;
        stdout.flush()?;
    }

    // For PTY-capable targets: automatically upgrade to a PTY and enter raw
    // mode, unless the transport already arrives with one (SSH, K8s).
    // stty sets the terminal size; then we spawn a pty shell via python3, python,
    // or script — whichever is available.  Even if all three fail the raw-mode
    // passthrough still works; interactive programs just won't have full PTY
    // semantics (user can type `upgrade` again later, or `pty` to re-enter raw mode).
    //
    // Transports without a PTY (WinRM) and Windows targets skip the upgrade
    // and use line-editor mode instead.
    let auto_raw = shell_type != ShellType::Windows && transport.caps.has_pty;
    let needs_upgrade = auto_raw && !transport.caps.starts_with_pty;
    if needs_upgrade {
        // Send PTY upgrade command. Do NOT include `stty` here — the PTY
        // isn't ready yet and stty will fail with "Inappropriate ioctl for
        // device". The delayed stty timer (1.5 s) handles sizing later.
        let upgrade = concat!(
            "python3 -c 'import pty; pty.spawn(\"/bin/bash\")' 2>/dev/null || ",
            "python -c 'import pty; pty.spawn(\"/bin/bash\")' 2>/dev/null || ",
            "script -qc /bin/bash /dev/null 2>/dev/null"
        );
        let _ = writer.write_all(upgrade.as_bytes()).await;
        let _ = writer.write_all(b"\n").await;
        let _ = writer.flush().await;
        write!(stdout, "\x1b[2m  [PTY mode — upload/download work directly; CTRL+Q for handler]\x1b[0m\r\n")?;
        stdout.flush()?;
    } else if auto_raw && transport.caps.starts_with_pty {
        // Transport already has a server-side PTY — no upgrade needed.
        write!(stdout, "\x1b[2m  [PTY mode — upload/download work directly; CTRL+Q for handler]\x1b[0m\r\n")?;
        stdout.flush()?;
        let _ = writer.write_all(b"\n").await;
        let _ = writer.flush().await;
    } else {
        // Windows: send a newline to get the first PS1.
        let _ = writer.write_all(b"\n").await;
        let _ = writer.flush().await;
    }

    let mut prompt_display = String::new();
    let mut prompt_vis = String::new();
    let mut raw_mode = auto_raw;   // start in raw PTY passthrough for Linux/Auto
    let mut raw_last_was_cr = false;
    let mut raw_line_buf: Vec<u8> = Vec::new();
    let mut ctrl_c_exit_armed = false; // true after first CTRL+C; second CTRL+C exits
    let mut display_state = DisplayState::default();
    // EventStream panics without a TTY ("reader source not set"), so only
    // create it when we have a local terminal (non-headless). Headless
    // daemons never poll keyboard input.
    let mut events: Option<EventStream> = if headless { None } else { Some(EventStream::new()) };
    // Defensive: a detached (background) session must never re-stop itself
    // when something touches the controlling tty. With SIGTTIN/SIGTTOU
    // ignored, background tty reads/writes fail with an error instead of
    // suspending the process.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    let mut editor = LineEditor::new();
    let mut net_buf = vec![0u8; 8192];
    let mut state = SessionState::Interactive;
    // Watchdog deadline for AgentCollecting state.  Set when a command is
    // injected, cleared when output is fully collected.  A 300-second timeout
    // prevents a hung remote command from growing the buffer without bound.
    let mut agent_deadline: Option<tokio::time::Instant> = None;
    let mut should_exit = false;
    // Detach/attach state.
    let mut terminal_active = !headless;
    let mut attached_reader: Option<tokio::net::unix::OwnedReadHalf> = None;
    let mut attached_writer: Option<tokio::net::unix::OwnedWriteHalf> = None;
    let mut attach_buf = vec![0u8; 4096];

    // After auto-upgrade, pty.spawn creates a new PTY that doesn't inherit the
    // stty size we set on the outer shell.  Fire a one-shot stty 1.5 s after
    // connect — by then pty.spawn's bash is definitely running.
    let initial_stty_at = tokio::time::Instant::now() + Duration::from_millis(1500);
    let initial_stty_fut = tokio::time::sleep_until(initial_stty_at);
    tokio::pin!(initial_stty_fut);
    // Skip the timer when headless — no terminal to read size from.
    let mut initial_stty_done = headless || !auto_raw;

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
                        // Suppress stty errors — the remote may not have a
                        // fully initialized PTY yet.
                        let _ = writer.write_all(
                            format!("stty cols {w} rows {h} 2>/dev/null\r").as_bytes()
                        ).await;
                        let _ = writer.flush().await;
                    }
                }
            }

            // ── Keyboard (only when the local terminal is active) ─────────────
            event_result = async {
                match events.as_mut() {
                    Some(ev) => ev.next().await,
                    None => std::future::pending().await,
                }
            }, if terminal_active => {
                match event_result {
                    Some(Ok(Event::Resize(w, h))) => {
                        if raw_mode {
                            let _ = writer.write_all(
                                format!("stty cols {w} rows {h} 2>/dev/null\r").as_bytes()
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
                            let (exit, maybe_meta) = handle_raw_key(
                                key, &mut raw_mode, &mut raw_last_was_cr,
                                &mut raw_line_buf,
                                &mut writer, &mut stdout,
                                &mut prompt_display, &mut prompt_vis,
                                &mut editor, &mut display_state,
                            ).await?;
                            should_exit = exit;
                            if let Some(action) = maybe_meta {
                                execute_raw_meta(
                                    action, &mut writer, &mut reader, &mut stdout,
                                    &mut terminal_active, session_id, shell_type,
                                ).await?;
                                if !terminal_active {
                                    raw_guard.take();
                                } else if let Ok((w, h)) = terminal::size() {
                                    let _ = writer.write_all(
                                        format!("stty cols {w} rows {h} 2>/dev/null\r").as_bytes(),
                                    ).await;
                                    let _ = writer.flush().await;
                                }
                            }
                        } else {
                            let prev_raw = raw_mode;
                            should_exit = handle_le_key(
                                key, &mut state, &mut writer, &mut stdout,
                                &mut prompt_display, &mut prompt_vis,
                                &mut editor, &mut display_state,
                                &mut meta, &mut raw_mode, &mut reader,
                                &mut ctrl_c_exit_armed,
                                &mut terminal_active, &mut events, session_id, shell_type,
                                transport.caps,
                            ).await?;
                            // Clear shadow buffer when switching into raw mode.
                            if !prev_raw && raw_mode {
                                raw_line_buf.clear();
                            }
                            // Detach action: release the terminal immediately.
                            if !terminal_active {
                                raw_guard.take();
                            }
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
                    if terminal_active {
                        write!(stdout, "\r\n{}\r\n", prompt::banner_disconnected())?;
                        stdout.flush()?;
                    } else if let Some(ref mut aw) = attached_writer {
                        let msg = format!("\r\n{}\r\n", prompt::banner_attach_session_closed());
                        let _ = aw.write_all(msg.as_bytes()).await;
                    }
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

                // Guard: if the agent buffer has grown beyond 64 MiB, send an error
                // and reset state immediately rather than letting it grow further.
                {
                    const AGENT_BUFFER_CAP: usize = 64 * 1024 * 1024;
                    let overflowed = matches!(&state,
                        SessionState::AgentCollecting { buffer, .. } if buffer.len() > AGENT_BUFFER_CAP
                    );
                    if overflowed {
                        agent_deadline = None;
                        if let SessionState::AgentCollecting { response_tx, .. } =
                            std::mem::replace(&mut state, SessionState::Interactive)
                        {
                            let _ = response_tx.send(AgentResponse {
                                output: "ERROR: agent command output exceeded 64 MiB limit\n".to_string(),
                                exit_code: 1,
                            });
                        }
                    }
                }

                // Update agent buffer (ANSI-stripped) and detect the done marker.
                // This runs regardless of terminal/attach state so that exec works
                // even while the session is detached.
                let agent_result: Option<(String, i32)> =
                    if let SessionState::AgentCollecting { nonce, buffer, pty_mode, .. } = &mut state {
                        buffer.push_str(&clean_str);
                        let done_marker = format!("SH_CMD_DONE_{}:", nonce);

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

                if terminal_active {
                    // Local terminal I/O — existing behaviour.
                    if raw_mode {
                        if matches!(state, SessionState::AgentCollecting { .. }) {
                            if !disp_to_show.is_empty() {
                                stdout.write_all(&raw_normalize(disp_to_show.as_bytes()))?;
                                stdout.flush()?;
                            }
                        } else {
                            stdout.write_all(&raw_normalize(&net_buf[..n]))?;
                            stdout.flush()?;
                        }
                    } else {
                        let has_user_input = !editor.buffer_str().is_empty();
                        let is_agent = matches!(state, SessionState::AgentCollecting { .. });
                        if has_user_input || is_agent {
                            clear_input(&mut stdout, display_state)?;
                        }
                        write!(stdout, "{}", disp_to_show.replace('\n', "\r\n"))?;
                        if !ends_with_nl {
                            prompt_display = last_line(&disp_to_show);
                            prompt_vis = last_line(&clean_str);
                        }
                        stdout.flush()?;
                    }
                } else if let Some(ref mut aw) = attached_writer {
                    // Relay raw bytes to the attached interactive client.
                    let _ = aw.write_all(&net_buf[..n]).await;
                    let _ = aw.flush().await;
                }
                // else: detached with no attached client → silently drain to keep TCP alive.

                // Finalise agent command if done marker was found.
                if let Some((output, exit_code)) = agent_result {
                    if let SessionState::AgentCollecting { response_tx, .. } =
                        std::mem::replace(&mut state, SessionState::Interactive)
                    {
                        let _ = response_tx.send(AgentResponse { output, exit_code });
                    }
                    agent_deadline = None;
                    if terminal_active && !raw_mode {
                        display_state = redraw_input(
                            &mut stdout, &prompt_display, &prompt_vis, &editor, None,
                        )?;
                    }
                } else if matches!(state, SessionState::AgentCollecting { .. }) && terminal_active && !raw_mode {
                    if !ends_with_nl {
                        write!(stdout, "\r\n")?;
                    }
                    write!(stdout, "\x1b[2m  [agent running...]\x1b[0m")?;
                    stdout.flush()?;
                } else if terminal_active && !raw_mode {
                    if !ends_with_nl {
                        if !editor.buffer_str().is_empty() {
                            display_state = redraw_input(
                                &mut stdout, &prompt_display, &prompt_vis, &editor, None,
                            )?;
                        } else {
                            display_state = DisplayState::default();
                        }
                    } else {
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
                    if terminal_active {
                        write!(stdout, "\r\n\x1b[1;33m[!]\x1b[0m session killed by remote caller\r\n")?;
                        stdout.flush()?;
                    }
                    break;
                }

                if matches!(state, SessionState::AgentCollecting { .. }) {
                    let _ = response_tx.send(AgentResponse {
                        output: "ERROR: session busy with another agent command\n".to_string(),
                        exit_code: 1,
                    });
                    continue;
                }

                // Block agent commands while an interactive client is attached.
                if attached_writer.is_some() {
                    let _ = response_tx.send(AgentResponse {
                        output: "ERROR: session has interactive client attached\n".to_string(),
                        exit_code: 1,
                    });
                    continue;
                }

                // Upload / download via IPC — run the transfer protocol directly on
                // the TCP stream.  Blocks the select! loop for the duration.
                if let Some(rest) = cmd.strip_prefix(agent::UPLOAD_CMD_PREFIX) {
                    if let Some(null_pos) = rest.find('\x00') {
                        let local_path = rest[..null_pos].to_string();
                        let remote_path = rest[null_pos + 1..].to_string();
                        if terminal_active {
                            if raw_mode {
                                write!(stdout, "\r\n{}\r\n", prompt::banner_agent_cmd(&format!("upload {} → {}", local_path, remote_path)))?;
                            } else {
                                clear_input(&mut stdout, display_state)?;
                                write!(stdout, "{}\r\n", prompt::banner_agent_cmd(&format!("upload {} → {}", local_path, remote_path)))?;
                            }
                            stdout.flush()?;
                        }
                        let result = transfer::upload(&mut reader, &mut writer, &local_path, &remote_path, &mut stdout, shell_type).await;
                        let (output, exit_code) = match result {
                            Ok(()) => ("upload complete\n".to_string(), 0),
                            Err(e) => (format!("upload failed: {e}\n"), 1),
                        };
                        let _ = response_tx.send(AgentResponse { output, exit_code });
                        if terminal_active {
                            if raw_mode {
                                if let Ok((w, h)) = terminal::size() {
                                    let _ = writer.write_all(format!("stty cols {w} rows {h} 2>/dev/null\r").as_bytes()).await;
                                    let _ = writer.flush().await;
                                }
                            } else {
                                display_state = redraw_input(&mut stdout, &prompt_display, &prompt_vis, &editor, None)?;
                            }
                        }
                    } else {
                        let _ = response_tx.send(AgentResponse {
                            output: "upload: malformed IPC command\n".to_string(),
                            exit_code: 1,
                        });
                    }
                    continue;
                }

                if let Some(rest) = cmd.strip_prefix(agent::DOWNLOAD_CMD_PREFIX) {
                    if let Some(null_pos) = rest.find('\x00') {
                        let remote_path = rest[..null_pos].to_string();
                        let local_path = rest[null_pos + 1..].to_string();
                        if terminal_active {
                            if raw_mode {
                                write!(stdout, "\r\n{}\r\n", prompt::banner_agent_cmd(&format!("download {} → {}", remote_path, local_path)))?;
                            } else {
                                clear_input(&mut stdout, display_state)?;
                                write!(stdout, "{}\r\n", prompt::banner_agent_cmd(&format!("download {} → {}", remote_path, local_path)))?;
                            }
                            stdout.flush()?;
                        }
                        let result = transfer::download(&mut reader, &mut writer, &remote_path, &local_path, &mut stdout, shell_type).await;
                        let (output, exit_code) = match result {
                            Ok(()) => ("download complete\n".to_string(), 0),
                            Err(e) => (format!("download failed: {e}\n"), 1),
                        };
                        let _ = response_tx.send(AgentResponse { output, exit_code });
                        if terminal_active {
                            if raw_mode {
                                if let Ok((w, h)) = terminal::size() {
                                    let _ = writer.write_all(format!("stty cols {w} rows {h} 2>/dev/null\r").as_bytes()).await;
                                    let _ = writer.flush().await;
                                }
                            } else {
                                display_state = redraw_input(&mut stdout, &prompt_display, &prompt_vis, &editor, None)?;
                            }
                        }
                    } else {
                        let _ = response_tx.send(AgentResponse {
                            output: "download: malformed IPC command\n".to_string(),
                            exit_code: 1,
                        });
                    }
                    continue;
                }

                // Show the agent-command banner when the local terminal is active.
                // Module scripts (`RUN_CMD_PREFIX`) show their display name
                // instead of the raw payload. All commands run verbatim.
                let (payload, banner_cmd) = match cmd.strip_prefix(agent::RUN_CMD_PREFIX) {
                    Some(rest) => match rest.split_once('\x00') {
                        Some((display, raw)) => (raw.to_string(), display.to_string()),
                        None => {
                            let _ = response_tx.send(AgentResponse {
                                output: "run: malformed IPC command\n".to_string(),
                                exit_code: 1,
                            });
                            continue;
                        }
                    },
                    None => (cmd.clone(), cmd.clone()),
                };
                if terminal_active {
                    if raw_mode {
                        write!(stdout, "\r\n{}\r\n", prompt::banner_agent_cmd(&banner_cmd))?;
                        stdout.flush()?;
                    } else {
                        clear_input(&mut stdout, display_state)?;
                        display_state = DisplayState::default();
                        write!(stdout, "{}\r\n", prompt::banner_agent_cmd(&banner_cmd))?;
                        stdout.flush()?;
                    }
                }

                let nonce = crate::util::generate_nonce();
                let payload_cmd = payload;
                // Marker framing follows the remote dialect; the START marker
                // is included whenever leading noise is possible (raw PTY
                // echo, non-POSIX prompts, PTY-less transports).
                let full_cmd = agent_command(
                    transport.shell,
                    &payload_cmd,
                    &nonce,
                    raw_mode,
                    transport.caps.has_pty,
                );
                writer.write_all(full_cmd.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
                meta.record_cmd(&banner_cmd);

                state = SessionState::AgentCollecting {
                    nonce,
                    buffer: String::new(),
                    response_tx,
                    pty_mode: framed_output(raw_mode, transport.caps.has_pty, transport.shell),
                };
                // Arm the 300-second watchdog.
                agent_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(300));

                if terminal_active {
                    if raw_mode {
                        write!(stdout, "\x1b[2m  [agent running...]\x1b[0m\r\n")?;
                    } else {
                        write!(stdout, "\x1b[2m  [agent running...]\x1b[0m")?;
                    }
                    stdout.flush()?;
                }
            }

            // ── Agent command timeout ─────────────────────────────────────────
            // Fires 300 s after a command was injected if the done marker never
            // arrived.  A new sleep is created each select! iteration but
            // tokio::time::sleep_until is cheap and the timer only ticks once.
            _ = async {
                match agent_deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                agent_deadline = None;
                if let SessionState::AgentCollecting { response_tx, .. } =
                    std::mem::replace(&mut state, SessionState::Interactive)
                {
                    if terminal_active {
                        if raw_mode {
                            write!(stdout, "\r\n\x1b[1;33m[!]\x1b[0m agent command timed out (300 s)\r\n")?;
                        } else {
                            clear_input(&mut stdout, display_state)?;
                            display_state = DisplayState::default();
                            write!(stdout, "\x1b[1;33m[!]\x1b[0m agent command timed out (300 s)\r\n")?;
                        }
                        stdout.flush()?;
                        if !raw_mode {
                            display_state = redraw_input(&mut stdout, &prompt_display, &prompt_vis, &editor, None)?;
                        }
                    }
                    let _ = response_tx.send(AgentResponse {
                        output: "ERROR: agent command timed out (300 s)\n".to_string(),
                        exit_code: 124,
                    });
                }
            }

            // ── Attach request ────────────────────────────────────────────────
            Some(attach_stream) = attach_rx.recv() => {
                if matches!(state, SessionState::AgentCollecting { .. }) || attached_writer.is_some() {
                    // Busy — drop the stream; the client will see a closed connection.
                    drop(attach_stream);
                } else {
                    let (ar, aw) = attach_stream.into_split();
                    attached_reader = Some(ar);
                    attached_writer = Some(aw);
                    // Anchor the attach screen: raw-PTY shells won't redraw
                    // their prompt on their own, so a fresh PS1 must be
                    // coaxed — otherwise the new client sits on a blank
                    // screen with no indication the session is live.
                    if transport.caps.has_pty {
                        let _ = writer.write_all(b"\x1b[2k\r").await; // clear current line
                        let _ = writer.write_all(b"\n").await;        // fresh PS1
                        let _ = writer.flush().await;
                    }
                }
            }

            // ── Input from attached interactive client ─────────────────────────
            n = async {
                match attached_reader.as_mut() {
                    Some(ar) => ar.read(&mut attach_buf).await,
                    None => std::future::pending::<std::io::Result<usize>>().await,
                }
            } => {
                match n {
                    Ok(0) | Err(_) => {
                        // Client detached or closed.
                        attached_reader = None;
                        attached_writer = None;
                    }
                    Ok(n) => {
                        writer.write_all(&attach_buf[..n]).await?;
                        writer.flush().await?;
                    }
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Key handlers
// ---------------------------------------------------------------------------

/// Returns `(should_exit, intercepted_meta_action)`.
async fn handle_raw_key(
    key: KeyEvent,
    raw_mode: &mut bool,
    last_was_cr: &mut bool,
    raw_line_buf: &mut Vec<u8>,
    writer: &mut (dyn AsyncWrite + Unpin + Send),
    stdout: &mut io::Stdout,
    prompt_display: &mut String,
    prompt_vis: &mut String,
    editor: &mut LineEditor,
    display_state: &mut DisplayState,
) -> anyhow::Result<(bool, Option<meta::MetaAction>)> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // CTRL+] or CTRL+Q — exit raw mode, return to line-editor.
    let is_escape = (key.code == KeyCode::Char(']') && ctrl)
        || (key.code == KeyCode::Char('q') && ctrl);

    if is_escape {
        *raw_mode = false;
        *last_was_cr = false;
        raw_line_buf.clear();
        prompt_display.clear();
        prompt_vis.clear();
        write!(stdout, "\r\n\x1b[2m  [handler mode — 'pty' or 'upgrade' to return to raw]\x1b[0m\r\n")?;
        stdout.flush()?;
        *display_state = redraw_input(stdout, prompt_display, prompt_vis, editor, None)?;
        return Ok((false, None));
    }

    let bytes = key_to_bytes(key);

    // CTRL+L — clear screen only locally.
    if key.code == KeyCode::Char('l') && ctrl {
        queue!(stdout, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
        stdout.flush()?;
        writer.write_all(&[0x0c]).await?;
        writer.flush().await?;
        *last_was_cr = false;
        return Ok((false, None));
    }

    if !bytes.is_empty() {
        // Shadow line buffer: accumulate printable bytes so that when the user
        // presses Enter we can check for a meta-command (upload, download, etc.)
        // and intercept it locally without requiring a mode switch.
        match bytes.as_slice() {
            [b'\r'] => {
                if let Some(action) = parse_meta_raw(raw_line_buf) {
                    raw_line_buf.clear();
                    *last_was_cr = false;
                    // Cancel the chars already sent to the remote shell's input buffer.
                    writer.write_all(b"\x15").await?;
                    writer.flush().await?;
                    return Ok((false, Some(action)));
                }
                raw_line_buf.clear();
                *last_was_cr = true;
                writer.write_all(&bytes).await?;
                writer.flush().await?;
            }
            [b'\x15'] => {
                // CTRL+U — user cleared the line.
                raw_line_buf.clear();
                *last_was_cr = false;
                writer.write_all(&bytes).await?;
                writer.flush().await?;
            }
            [b'\x7f'] | [b'\x08'] => {
                // Backspace.
                raw_line_buf.pop();
                *last_was_cr = false;
                writer.write_all(&bytes).await?;
                writer.flush().await?;
            }
            [b] if (0x20u8..=0x7eu8).contains(b) => {
                // Printable ASCII.
                raw_line_buf.push(*b);
                *last_was_cr = false;
                writer.write_all(&bytes).await?;
                writer.flush().await?;
            }
            _ => {
                // ESC sequences, arrows, F-keys, etc. — cursor may have moved,
                // so tracking is unreliable; reset the buffer.
                raw_line_buf.clear();
                *last_was_cr = bytes.last() == Some(&b'\r');
                writer.write_all(&bytes).await?;
                writer.flush().await?;
            }
        }
    }
    Ok((false, None))
}

/// Returns `true` if the session should exit.
#[allow(clippy::too_many_arguments)]
async fn handle_le_key(
    key: KeyEvent,
    state: &mut SessionState,
    writer: &mut (dyn AsyncWrite + Unpin + Send),
    stdout: &mut io::Stdout,
    prompt_display: &mut String,
    prompt_vis: &mut String,
    editor: &mut LineEditor,
    display_state: &mut DisplayState,
    meta: &mut LiveMeta,
    raw_mode: &mut bool,
    reader: &mut (dyn AsyncRead + Unpin + Send),
    ctrl_c_exit_armed: &mut bool,
    terminal_active: &mut bool,
    events: &mut Option<EventStream>,
    session_id: &str,
    shell_type: ShellType,
    caps: TransportCaps,
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
        LineAction::Detach => {
            // CTRL+Z — detach locally, keep TCP alive. Same as the `detach`
            // meta-command: release the terminal and suspend so the shell
            // prompt returns immediately. User types 'bg' once, then
            // `kash attach <id>` from any terminal to reconnect.
            // Nothing is sent to the remote shell.
            *ctrl_c_exit_armed = false;
            cancel_agent(state, 130);
            write!(
                stdout,
                "\r\n{}\r\n",
                prompt::banner_session_detached(session_id)
            )?;
            stdout.flush()?;
            *terminal_active = false;
            // Stop polling the local terminal: once this process runs in the
            // background, reading the controlling tty raises SIGTTIN and
            // re-stops the process (exec/attach then never see a reply).
            *events = events.take();
            #[cfg(unix)]
            unsafe {
                libc::raise(libc::SIGTSTP);
            }
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
                Some(meta::MetaAction::Help) => {
                    write!(stdout, "{}", prompt::help_text().replace('\n', "\r\n"))?;
                    stdout.flush()?;
                    *display_state =
                        redraw_input(stdout, prompt_display, prompt_vis, editor, None)?;
                }
                Some(meta::MetaAction::Clear) => {
                    queue!(stdout, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
                    *display_state =
                        redraw_input(stdout, prompt_display, prompt_vis, editor, None)?;
                }
                Some(meta::MetaAction::Download { remote, local }) => {
                    if let Err(e) = transfer::download(reader, writer, &remote, &local, stdout, shell_type).await {
                        write!(stdout, "{}\r\n", prompt::banner_error(&e.to_string()))?;
                    }
                    *display_state =
                        redraw_input(stdout, prompt_display, prompt_vis, editor, None)?;
                }
                Some(meta::MetaAction::Upload { local, remote }) => {
                    if let Err(e) = transfer::upload(reader, writer, &local, &remote, stdout, shell_type).await {
                        write!(stdout, "{}\r\n", prompt::banner_error(&e.to_string()))?;
                    }
                    *display_state =
                        redraw_input(stdout, prompt_display, prompt_vis, editor, None)?;
                }
                Some(meta::MetaAction::Pty) => {
                    if !caps.has_pty {
                        // PTY-less transports (WinRM) have no raw mode.
                        write!(stdout, "\x1b[2m  [no PTY on this transport — staying in handler mode]\x1b[0m\r\n")?;
                        stdout.flush()?;
                        *display_state =
                            redraw_input(stdout, prompt_display, prompt_vis, editor, None)?;
                        return Ok(false);
                    }
                    // Switch to raw PTY passthrough — no upgrade sent.
                    write!(
                        stdout,
                        "\x1b[2m  [raw PTY mode — CTRL+] to return to handler]\x1b[0m\r\n"
                    )?;
                    stdout.flush()?;
                    if let Ok((w, h)) = terminal::size() {
                        let _ = writer
                            .write_all(format!("stty cols {w} rows {h} 2>/dev/null\r").as_bytes())
                            .await;
                        let _ = writer.flush().await;
                    }
                    *raw_mode = true;
                    prompt_display.clear();
                    prompt_vis.clear();
                    *display_state = DisplayState::default();
                }
                Some(meta::MetaAction::Upgrade) => {
                    if !caps.has_pty {
                        write!(stdout, "\x1b[2m  [no PTY on this transport — upgrade unavailable]\x1b[0m\r\n")?;
                        stdout.flush()?;
                        *display_state =
                            redraw_input(stdout, prompt_display, prompt_vis, editor, None)?;
                        return Ok(false);
                    }
                    // Send PTY upgrade then switch to raw mode.
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
                            .write_all(format!("stty cols {w} rows {h} 2>/dev/null\r").as_bytes())
                            .await;
                        let _ = writer.flush().await;
                    }
                    write!(
                        stdout,
                        "\x1b[2m  [raw PTY mode — CTRL+] to return to handler]\x1b[0m\r\n"
                    )?;
                    stdout.flush()?;
                }
                Some(meta::MetaAction::Detach) => {
                    write!(
                        stdout,
                        "\r\n{}\r\n",
                        prompt::banner_session_detached(session_id)
                    )?;
                    stdout.flush()?;
                    // Release the terminal and suspend this process so the shell
                    // prompt returns immediately.  The user types 'bg' once and the
                    // session keeps running in the background.
                    *terminal_active = false;
                    // Same SIGTTIN guard as the CTRL+Z detach path above.
                    *events = None;
                    #[cfg(unix)]
                    unsafe {
                        libc::raise(libc::SIGTSTP);
                    }
                }
                None => {
                    if !trimmed.is_empty() {
                        // Commands are sent verbatim.
                        writer.write_all(line.as_bytes()).await?;
                        writer.write_all(b"\n").await?;
                        writer.flush().await?;
                        meta.record_cmd(&trimmed);
                    }
                    // No redraw — remote shell will send its own PS1.
                }
            }
        }

        LineAction::SendRaw(bytes) => {
            queue!(stdout, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            write!(stdout, "\x1b[2m^Z\x1b[0m\r\n")?;
            stdout.flush()?;
            *display_state = DisplayState::default();
            writer.write_all(&bytes).await?;
            writer.flush().await?;
        }

        // ClearScreen, Disconnect and Detach are handled before this match block.
        LineAction::ClearScreen | LineAction::Disconnect | LineAction::Detach => unreachable!(),
    }

    Ok(false)
}

/// Reply to a pending agent command and reset to [`SessionState::Interactive`].
/// `exit_code` 130 marks a locally cancelled command (CTRL+C), other codes
/// pass through; no-op when no command is being collected.
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
// Utilities
// ---------------------------------------------------------------------------

/// Send a dialect-appropriate `whoami/hostname` probe and wait (≤2 s) for a
/// `SHIDENTITY:<user>:<host>` line in the response, tolerating prompt noise
/// around it. Falls back to `"?"`/`"?"` on timeout, failure, or a dialect
/// that can't emit a usable probe.
async fn probe_identity(
    reader: &mut (dyn AsyncRead + Unpin + Send),
    writer: &mut (dyn AsyncWrite + Unpin + Send),
    shell: RemoteShell,
) -> (String, String) {
    let probe = probe_command(shell);
    if writer.write_all(probe.as_bytes()).await.is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{TransportCaps, TransportKind};

    #[test]
    fn reverse_caps_need_upgrade() {
        let caps = TransportCaps::reverse();
        assert!(caps.has_pty && !caps.starts_with_pty && caps.supports_resize);
    }

    #[test]
    fn transport_info_carries_peer() {
        let info = TransportInfo::new(TransportKind::Ssh, "root@h:22", TransportCaps::pty());
        assert_eq!(info.peer, "root@h:22");
    }
}
