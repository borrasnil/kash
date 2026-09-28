//! Windows / PowerShell file transfer.
//!
//! Uses `[IO.File]::ReadAllBytes` + `[Convert]::ToBase64String` — no external
//! tools required, works on all PowerShell versions.  Works whether the current
//! shell is cmd.exe (via `powershell -NoP -c`) or PowerShell directly.

use std::io::{self, Write as _};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::protocol::*;
use crate::util::gen_temp_nonce;

/// Download a remote file from a Windows/PowerShell target.
pub(super) async fn download_windows<R, W>(
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

    write!(stdout, "  \x1b[2m[↓]\x1b[0m  {} → {}   0 B\r", remote_path, local_path)?;
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

/// Upload a local file to a Windows/PowerShell target.
///
/// Streams base64 data using `Add-Content` per chunk (no heredoc equivalent
/// on Windows).  Chunk size is larger than Linux to reduce round-trips.
/// Works whether the current shell is cmd.exe or PowerShell; the commands
/// use only .NET APIs available on all PowerShell versions.
pub(super) async fn upload_windows<R, W>(
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
        write!(stdout, "  \x1b[2m[↑]\x1b[0m  {} → {}   0 B  \x1b[2m0%\x1b[0m\r", local_path, remote_path)?;
    } else {
        write!(stdout, "  \x1b[2m[↑]\x1b[0m  {} → {}   0 B\r", local_path, remote_path)?;
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
                let b64 = crate::util::base64_encode(&raw_chunk[..n]);
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
                            "  \x1b[2m[↑]\x1b[0m  {} → {}   {}  \x1b[2m{}%\x1b[0m\r",
                            local_path, remote_path, fmt_bytes(bytes_sent), pct
                        )?;
                    } else {
                        write!(stdout, "  \x1b[2m[↑]\x1b[0m  {} → {}   {}\r", local_path, remote_path, fmt_bytes(bytes_sent))?;
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
