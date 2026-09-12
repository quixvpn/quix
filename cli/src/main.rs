mod commands;
mod service_manager;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	commands::run().await
}