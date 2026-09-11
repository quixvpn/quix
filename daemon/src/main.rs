mod handler;
mod identity;

use anyhow::Result;
use iroh::Endpoint;
use iroh::endpoint::presets;
use iroh::protocol::Router;

use handler::Echo;

const ALPN: &[u8] = b"quix-vpn/0";

#[tokio::main]
async fn main() -> Result<()> {
	let secret_key = identity::load_or_create()?;

	let endpoint = Endpoint::builder(presets::N0)
		.secret_key(secret_key)
		.bind()
		.await?;

	println!("quixd listening, id: {}", endpoint.id());

	let router = Router::builder(endpoint)
		.accept(ALPN, Echo)
		.spawn();

	tokio::signal::ctrl_c().await?;
	router.shutdown().await?;

	Ok(())
}