use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use shell_handler::cli::{ObfuscationLevel, ShellType};
use shell_handler::obfuscation::create_strategy;
use shell_handler::output::clean_output;

#[tokio::test]
async fn obfuscated_command_over_tcp() {
    // Bind a listener (simulating our tool waiting for a victim)
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Spawn the "tool side": accept connection, obfuscate a command, send it
    let tool = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.unwrap();
        let engine = create_strategy(ObfuscationLevel::Medium, ShellType::Linux);
        let cmd = engine.obfuscate("echo hello");
        socket.write_all(cmd.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
    });

    // Simulate the victim connecting and receiving the obfuscated command
    let mut victim = TcpStream::connect(addr).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = victim.read(&mut buf).await.unwrap();

    let received = clean_output(&buf[..n]);
    assert_ne!(received, "echo hello", "command should be obfuscated");
    assert!(!received.is_empty());

    tool.await.unwrap();
}
