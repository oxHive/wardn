#[tokio::main]
async fn main() -> anyhow::Result<()> {
    wardn::cli::run().await
}
