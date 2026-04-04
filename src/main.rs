#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sendspin_rs_cli::run().await
}
