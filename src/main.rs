/// Binary entry point: async-dispatch to [`kash::run`] and print errors with
/// `{e:#}` so the anyhow context chain is readable on stderr.
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
