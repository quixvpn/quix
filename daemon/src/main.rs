mod connect;
mod handler;
mod identity;
mod ipc;

use anyhow::Result;
use iroh::Endpoint;
use iroh::endpoint::presets;
use iroh::protocol::Router;

use handler::Echo;

pub const ALPN: &[u8] = b"quix-vpn/0";

#[tokio::main]
async fn main() -> Result<()> {
	let secret_key = identity::load_or_create()?;

	let endpoint = Endpoint::builder(presets::N0)
		.secret_key(secret_key)
		.bind()
		.await?;

	println!("quixd listening, id: {}", endpoint.id());

	let router = Router::builder(endpoint.clone())
		.accept(ALPN, Echo)
		.spawn();

	ipc::serve(endpoint).await?; // blocks, serving CLI commands

	router.shutdown().await?;
	Ok(())
}