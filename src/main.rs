use sendspin_rs_cli::error::SendspinError;

#[tokio::main]
async fn main() -> Result<(), SendspinError> {
    sendspin_rs_cli::run().await
}
