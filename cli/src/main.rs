mod commands;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	commands::run().await
}