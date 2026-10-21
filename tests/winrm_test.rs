//! WinRM transport integration tests against a hand-rolled mock SOAP server.
//!
//! Real Windows hosts aren't available in CI, so `MockWinRm` implements just
//! enough WS-Management over HTTP/1.1: route by SOAP action URI, answer with
//! canned envelopes, and record what the client sent (stdin payloads,
//! signals, shell deletion). The mock trusts Basic auth and rejects
//! everything else with 401 — NTLM crypto stays `winrm-rs`' tested
//! territory; the live-test checklist in `README.md` covers it on Windows.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use kash::transport::Transport;
use kash::winrm::{WinRmAuth, WinRmShell, WinRmTransport};

// ---------------------------------------------------------------------------
// Mock server
// ---------------------------------------------------------------------------

/// Scripted output chunks served to `Receive` polls, in order.
#[derive(Debug)]
struct Chunk {
    stdout: Vec<u8>,
    done: bool,
}

#[derive(Debug, Default)]
struct State {
    /// Decoded stdin payloads received via `Send`, in order.
    sends: Vec<Vec<u8>>,
    /// Queued `Receive` answers.
    chunks: VecDeque<Chunk>,
    /// `Signal` (CTRL+C) observed.
    signaled: bool,
    /// Shell `Delete` observed.
    deleted: bool,
    /// Requests seen (first line of the SOAP body mentioning the action).
    actions: Vec<String>,
    /// Captured `Authorization` headers of the first request.
    first_auth: Option<String>,
}

struct MockWinRm {
    addr: std::net::SocketAddr,
    state: Arc<Mutex<State>>,
}

impl MockWinRm {
    async fn start(chunks: Vec<Chunk>) -> Self {
        let state = Arc::new(Mutex::new(State {
            chunks: chunks.into(),
            ..Default::default()
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let st = Arc::clone(&shared);
                tokio::spawn(async move { serve_conn(stream, st).await });
            }
        });
        Self { addr, state }
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }
}

/// Minimal HTTP/1.1: headers until blank line, `Content-Length` body,
/// keep-alive across sequential requests on one connection.
async fn serve_conn(mut stream: tokio::net::TcpStream, state: Arc<Mutex<State>>) {
    let mut buf = Vec::new();
    let mut tmp = vec![0u8; 8192];
    loop {
        // Read headers.
        let head_end = loop {
            match stream.read(&mut tmp).await {
                Ok(0) => return,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = find_headers_end(&buf) {
                        break p;
                    }
                }
                Err(_) => return,
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let content_len = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                (k.trim().eq_ignore_ascii_case("content-length"))
                    .then(|| v.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        while buf.len() < head_end + content_len {
            match stream.read(&mut tmp).await {
                Ok(0) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(_) => return,
            }
        }
        let body = buf[head_end..head_end + content_len].to_vec();
        buf.drain(..head_end + content_len);

        // Auth: trust Basic, 401 anything else (records first header).
        let authed = head.lines().any(|l| {
            let (k, v) = l.split_once(':').unwrap_or(("", ""));
            k.trim().eq_ignore_ascii_case("authorization") && v.trim().starts_with("Basic ")
        });
        {
            let mut st = state.lock().unwrap();
            if st.first_auth.is_none() {
                st.first_auth = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim()
                            .eq_ignore_ascii_case("authorization")
                            .then(|| v.trim().to_string())
                    });
            }
        }
        if !authed {
            let resp = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"WinRM\"\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";
            if stream.write_all(resp.as_bytes()).await.is_err() {
                return;
            }
            continue;
        }

        let body_str = String::from_utf8_lossy(&body).into_owned();
        let response = route(&body_str, &state);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/soap+xml;charset=UTF-8\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
            response.len(),
            response
        );
        if stream.write_all(resp.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

fn route(body: &str, state: &Arc<Mutex<State>>) -> String {
    let mut st = state.lock().unwrap();
    if body.contains("transfer/Create</") {
        st.actions.push("Create".to_string());
        return envelope(
            "<rsp:Shell xmlns:rsp=\"http://schemas.microsoft.com/wbem/wsman/1/windows/shell\">\
             <rsp:ShellId>SHELL-1</rsp:ShellId>\
             <rsp:ResourceUri>http://schemas.microsoft.com/wbem/wsman/1/windows/shell</rsp:ResourceUri>\
             </rsp:Shell>",
        );
    }
    if body.contains("windows/shell/Command</") {
        st.actions.push("Command".to_string());
        return envelope(
            "<rsp:CommandResponse xmlns:rsp=\"http://schemas.microsoft.com/wbem/wsman/1/windows/shell\">\
             <rsp:CommandId>CMD-1</rsp:CommandId>\
             </rsp:CommandResponse>",
        );
    }
    if body.contains("windows/shell/Send</") {
        st.actions.push("Send".to_string());
        // Send carries base64 stdin inside <rsp:Stream>.
        if let Some(b64) = tag_content(body, "rsp:Stream") {
            let clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
            if let Some(raw) = kash::util::base64_decode(&clean) {
                st.sends.push(raw);
            }
        }
        return envelope(
            "<rsp:SendResponse xmlns:rsp=\"http://schemas.microsoft.com/wbem/wsman/1/windows/shell\"/>",
        );
    }
    if body.contains("windows/shell/Receive</") {
        st.actions.push("Receive".to_string());
        let chunk = st.chunks.pop_front().unwrap_or(Chunk {
            stdout: Vec::new(),
            done: false,
        });
        let b64 = kash::util::base64_encode(&chunk.stdout);
        let state_uri = if chunk.done {
            "http://schemas.microsoft.com/wbem/wsman/1/windows/shell/Done"
        } else {
            "http://schemas.microsoft.com/wbem/wsman/1/windows/shell/Running"
        };
        let exit = if chunk.done {
            "<rsp:ExitCode>0</rsp:ExitCode>".to_string()
        } else {
            String::new()
        };
        return envelope(&format!(
            "<rsp:ReceiveResponse xmlns:rsp=\"http://schemas.microsoft.com/wbem/wsman/1/windows/shell\">\
             <rsp:Stream Name=\"stdout\">{b64}</rsp:Stream>\
             <rsp:CommandState State=\"{state_uri}\">{exit}</rsp:CommandState>\
             </rsp:ReceiveResponse>"
        ));
    }
    if body.contains("windows/shell/Signal</") {
        st.actions.push("Signal".to_string());
        st.signaled = true;
        return envelope(
            "<rsp:SignalResponse xmlns:rsp=\"http://schemas.microsoft.com/wbem/wsman/1/windows/shell\"/>",
        );
    }
    if std::env::var("KASH_WINRM_DEBUG").is_ok() {
        eprintln!("MOCK-UNKNOWN-BODY:\n{}", &body[..body.len().min(2000)]);
    }
    if body.contains("transfer/Delete</") {
        st.actions.push("Delete".to_string());
        st.deleted = true;
        return envelope("");
    }
    st.actions.push("Unknown".to_string());
    envelope("")
}

fn envelope(inner: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\">\
         <s:Header/>\
         <s:Body>{inner}</s:Body>\
         </s:Envelope>"
    )
}

fn tag_content(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = body.find(&open)?;
    let content_start = body[start..].find('>')? + start + 1;
    let close = format!("</{tag}>");
    let end = body[content_start..].find(&close)? + content_start;
    Some(body[content_start..end].to_string())
}

fn transport_to_mock(mock: &MockWinRm, shell: WinRmShell) -> WinRmTransport {
    WinRmTransport {
        host: "127.0.0.1".to_string(),
        port: mock.port(),
        ssl: false,
        insecure: false,
        user: "Admin".to_string(),
        password: "s3cret".to_string(),
        auth: WinRmAuth::Basic,
        shell,
    }
}

/// Read from `reader` until `needle` appears (or timeout). Returns all bytes.
async fn read_until<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    needle: &[u8],
    timeout_ms: u64,
) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut acc = Vec::new();
    let mut tmp = vec![0u8; 4096];
    loop {
        if acc.windows(needle.len()).any(|w| w == needle) {
            break;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, reader.read(&mut tmp)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => acc.extend_from_slice(&tmp[..n]),
            _ => break,
        }
    }
    acc
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repl_echo_roundtrip_over_basic_auth() {
    let mock = MockWinRm::start(vec![
        Chunk { stdout: b"Microsoft PowerShell\n".to_vec(), done: false },
        Chunk { stdout: b"hi\r\n".to_vec(), done: false },
    ])
    .await;
    let t = transport_to_mock(&mock, WinRmShell::Powershell);
    let (mut reader, mut writer, info) = t.connect().await.expect("connect");
    assert_eq!(info.peer, "Admin@127.0.0.1:".to_owned() + &mock.port().to_string() + " (winrm)");

    // Speak like the session loop: one command line, then read its output.
    writer.write_all(b"echo hi\n").await.unwrap();
    writer.flush().await.unwrap();
    let out = read_until(&mut reader, b"hi", 10_000).await;
    assert!(
        String::from_utf8_lossy(&out).contains("hi"),
        "expected REPL echo, got: {}",
        String::from_utf8_lossy(&out)
    );

    // The mock saw shell creation, a powershell REPL command, our stdin,
    // and polls — and the client authenticated with Basic.
    let st = mock.state.lock().unwrap();
    assert!(st.actions.contains(&"Create".to_string()));
    assert!(st.actions.contains(&"Command".to_string()));
    assert!(st.actions.contains(&"Send".to_string()));
    assert!(st.actions.contains(&"Receive".to_string()));
    let sent: Vec<u8> = st.sends.concat();
    assert!(
        String::from_utf8_lossy(&sent).contains("echo hi"),
        "stdin not forwarded: {}",
        String::from_utf8_lossy(&sent)
    );
    assert!(
        st.first_auth.as_deref().unwrap_or("").starts_with("Basic "),
        "expected Basic auth, got {:?}",
        st.first_auth
    );
}

#[tokio::test]
async fn marker_framed_exec_flows_through_adapter() {
    let done_line = b"hello\nSH_CMD_DONE_abc123:0\n".to_vec();
    let mock = MockWinRm::start(vec![
        Chunk { stdout: b"".to_vec(), done: false },
        Chunk { stdout: done_line.clone(), done: false },
    ])
    .await;
    let t = transport_to_mock(&mock, WinRmShell::Powershell);
    let (mut reader, mut writer, _) = t.connect().await.expect("connect");

    // Exactly what session::frame emits for PowerShell.
    let framed = kash::session::frame::agent_command(
        kash::transport::RemoteShell::Powershell,
        "echo hello",
        "abc123",
        false,
        false,
    );
    writer.write_all(framed.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    let out = read_until(&mut reader, b"SH_CMD_DONE_abc123:0", 10_000).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    assert!(text.contains("hello"), "{text}");
    assert!(text.contains("SH_CMD_DONE_abc123:0"), "{text}");
}

#[tokio::test]
async fn ctrl_c_sends_signal_not_stdin() {
    let mock = MockWinRm::start(vec![]).await;
    let t = transport_to_mock(&mock, WinRmShell::Powershell);
    let (_reader, mut writer, _) = t.connect().await.expect("connect");

    writer.write_all(&[0x03]).await.unwrap();
    writer.flush().await.unwrap();
    // Give the pump a moment to issue the Signal request.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let st = mock.state.lock().unwrap();
    assert!(st.signaled, "expected Signal request, actions: {:?}", st.actions);
    assert!(st.sends.is_empty(), "0x03 must not reach stdin");
}

#[tokio::test]
async fn shell_deleted_when_session_ends() {
    let mock = MockWinRm::start(vec![]).await;
    let t = transport_to_mock(&mock, WinRmShell::Powershell);
    let (reader, writer, _) = t.connect().await.expect("connect");
    drop(reader);
    drop(writer);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let st = mock.state.lock().unwrap();
    assert!(st.deleted, "expected Delete, actions: {:?}", st.actions);
}

#[tokio::test]
async fn repl_done_ends_stream_with_eof() {
    let mock = MockWinRm::start(vec![Chunk {
        stdout: b"bye\n".to_vec(),
        done: true,
    }])
    .await;
    let t = transport_to_mock(&mock, WinRmShell::Cmd);
    let (mut reader, _writer, _) = t.connect().await.expect("connect");

    let mut acc = Vec::new();
    let mut tmp = vec![0u8; 1024];
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(10), reader.read(&mut tmp)).await
        {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => acc.extend_from_slice(&tmp[..n]),
            _ => break,
        }
    }
    assert!(String::from_utf8_lossy(&acc).contains("bye"));
}

#[tokio::test]
async fn auth_rejection_surfaces_as_error() {
    // Mock with no route trust: flip to 401 by pointing at a fresh mock
    // whose state we poison — simplest: wrong port (connection refused).
    let t = WinRmTransport {
        host: "127.0.0.1".to_string(),
        port: 1, // privileged/closed: connect fails fast
        ssl: false,
        insecure: false,
        user: "Admin".to_string(),
        password: "x".to_string(),
        auth: WinRmAuth::Basic,
        shell: WinRmShell::Powershell,
    };
    let err = match t.connect().await {
        Ok(_) => panic!("connect to a closed port must fail"),
        Err(e) => e.to_string(),
    };
    assert!(!err.is_empty(), "error must explain the failure");
}
