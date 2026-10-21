use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use kash::output::clean_output;

#[tokio::test]
async fn commands_are_sent_verbatim_over_tcp() {
    // Bind a listener (simulating our tool waiting for a victim)
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Spawn the "tool side": accept connection, send a command verbatim
    let tool = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.unwrap();
        let cmd = "echo hello";
        socket.write_all(cmd.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
    });

    // Simulate the victim connecting and receiving the command
    let mut victim = TcpStream::connect(addr).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = victim.read(&mut buf).await.unwrap();

    let received = clean_output(&buf[..n]);
    assert_eq!(received, "echo hello", "commands must reach the target verbatim");
    assert!(!received.is_empty());

    tool.await.unwrap();
}
