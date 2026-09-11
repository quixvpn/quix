mod admin;
mod connect;
mod handler;
mod identity;
mod ipc;
mod membership;
mod mesh;
mod peers;
mod state;
mod tun;

use admin::{AdminHandler, ADMIN_ALPN};
use anyhow::Result;
use handler::DataHandler;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::Endpoint;
use state::State;

pub const ALPN: &[u8] = b"quix-vpn/0";

/// Depth of the dialer's on-demand queue. Requests are dropped when it's full;
/// the dialer's periodic sweep covers anything missed.
const DIAL_QUEUE: usize = 64;

/// How long to wait for a relay at startup before carrying on regardless.
const ONLINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::main]
async fn main() -> Result<()> {
	let secret_key = identity::load_or_create()?;

	let endpoint = Endpoint::builder(presets::N0)
		.secret_key(secret_key)
		.bind()
		.await?;

	println!("quixd listening, id: {}", endpoint.id());

	// Our address is published to pkarr only once we have a relay, and an
	// invite carries nothing but an id — so until this completes, a joiner
	// dialing us resolves nothing and fails with "no addressing information".
	// Bounded, because a daemon with no connectivity should still come up and
	// answer `quix status`; the dialer retries once a relay appears.
	match tokio::time::timeout(ONLINE_TIMEOUT, endpoint.online()).await {
		Ok(()) => println!("online, reachable at: {:?}", endpoint.addr()),
		Err(_) => eprintln!(
			"warning: no relay after {}s — peers may not be able to find us yet",
			ONLINE_TIMEOUT.as_secs()
		),
	}

	let virtual_ip = tun::virtual_ipv4(endpoint.id().as_bytes());
	println!("virtual IP: {virtual_ip}");

	let tun_device = tun::create(virtual_ip)?;
	println!("tun device up: {}", tun::interface_name());

	let (dial_tx, dial_rx) = tokio::sync::mpsc::channel(DIAL_QUEUE);
	let state = State::new(endpoint.clone(), tun_device, dial_tx)?;

	// Seed the routing table from the roster on disk, which also kicks off
	// dials to everyone we already know about.
	state.refresh_routes().await;

	let router = Router::builder(endpoint.clone())
		.accept(ALPN, DataHandler { state: state.clone() })
		.accept(
			ADMIN_ALPN,
			AdminHandler {
				state: state.clone(),
			},
		)
		.spawn();

	tokio::spawn(mesh::tun_to_mesh(state.clone()));
	tokio::spawn(mesh::dialer(state.clone(), dial_rx));

	ipc::serve(state).await?;

	router.shutdown().await?;
	Ok(())
}
