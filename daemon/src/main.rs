mod connect;
mod handler;
mod identity;
mod ipc;
mod state;
mod tun;

use anyhow::Result;
use iroh::Endpoint;
use iroh::endpoint::presets;
use iroh::protocol::Router;

use handler::Echo;
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

    let _tun_device = tun::create(virtual_ip)?;
    println!("tun device up: {}", tun::INTERFACE_NAME);

    let state = State::default();

	let router = Router::builder(endpoint.clone())
        .accept(ALPN, Echo { state: state.clone() })
        .spawn();

	ipc::serve(endpoint, state).await?; // blocks, serving CLI commands

	router.shutdown().await?;
	Ok(())
}