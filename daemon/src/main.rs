mod admin;
mod connect;
mod handler;
mod identity;
mod ipc;
mod membership;
mod state;
mod tun;

use admin::{AdminHandler, ADMIN_ALPN};
use anyhow::Result;
use handler::DataHandler;
use iroh::Endpoint;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use state::State;

pub const ALPN: &[u8] = b"quix-vpn/0";

#[tokio::main]
async fn main() -> Result<()> {
	let secret_key = identity::load_or_create()?;

	let endpoint = Endpoint::builder(presets::N0)
		.secret_key(secret_key)
		.bind()
		.await?;

	println!("quixd listening, id: {}", endpoint.id());

	let virtual_ip = tun::virtual_ipv4(endpoint.id().as_bytes());
	println!("virtual IP: {virtual_ip}");

	let tun_device = tun::create(virtual_ip)?;
	println!("tun device up: {}", tun::INTERFACE_NAME);

	let state = State::new(tun_device)?;

	let router = Router::builder(endpoint.clone())
		.accept(ALPN, DataHandler { state: state.clone() })
		.accept(ADMIN_ALPN, AdminHandler { state: state.clone() })
		.spawn();

	ipc::serve(endpoint, state).await?;

	router.shutdown().await?;
	Ok(())
}