#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let result = kash::run().await;

    match &result {
        Ok(()) => {}
        Err(e) => {
            let msg = format!("{e:#}");
            eprintln!("{msg}");
        }
    }

    result
}
