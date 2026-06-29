#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let result = shell_handler::run().await;

    match &result {
        Ok(()) => {}
        Err(e) => {
            let msg = format!("{e:#}");
            eprintln!("{msg}");
        }
    }

    result
}
