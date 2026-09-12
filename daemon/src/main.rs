mod admin;
mod authz;
mod connect;
mod dns;
mod handler;
mod identity;
mod ipc;
mod log;
mod membership;
mod mesh;
mod names;
mod peers;
mod resolv;
mod routes;
#[cfg(windows)]
mod service;
mod settings;
mod state;
mod stats;
mod tun;
#[cfg(windows)]
mod winacl;
#[cfg(windows)]
mod winauth;

use std::future::Future;

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

fn main() -> Result<()> {
	// Started by the Service Control Manager? Then it owns our lifecycle. The
	// dispatcher only connects inside a real service process, so a failure here
	// means we were launched from a terminal instead.
	#[cfg(windows)]
	if service::run() {
		return Ok(());
	}

	runtime()?.block_on(run(async {
		let _ = tokio::signal::ctrl_c().await;
		crate::info!("interrupted");
	}))
}

pub fn runtime() -> Result<tokio::runtime::Runtime> {
	Ok(tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()?)
}

/// Where names still resolve when OS integration could not be set up. Always
/// available, since it is the one endpoint the resolver refuses to start without.
fn fallback_resolver() -> String {
	dns::listen_addr()
		.map(|addr| addr.to_string())
		.unwrap_or_default()
}

/// Runs the daemon until `shutdown` resolves.
pub async fn run(shutdown: impl Future<Output = ()>) -> Result<()> {
	let secret_key = identity::load_or_create()?;

	let endpoint = Endpoint::builder(presets::N0)
		.secret_key(secret_key)
		.bind()
		.await?;

	if let Some(path) = log::path() {
		crate::info!("logging to {}", path.display());
	}
	crate::info!("quixd {} listening, id: {}", proto::VERSION_TAG, endpoint.id());
	crate::info!("state: {}", identity::key_path()?.display());

	// Our address is published to pkarr only once we have a relay, and an
	// invite carries nothing but an id — so until this completes, a joiner
	// dialing us resolves nothing and fails with "no addressing information".
	// Bounded, because a daemon with no connectivity should still come up and
	// answer `quix status`; the dialer retries once a relay appears.
	match tokio::time::timeout(ONLINE_TIMEOUT, endpoint.online()).await {
		Ok(()) => crate::info!("online, reachable at: {:?}", endpoint.addr()),
		Err(_) => crate::warn!(
			"warning: no relay after {}s — peers may not be able to find us yet",
			ONLINE_TIMEOUT.as_secs()
		),
	}

	let (v4, v6) = tun::virtual_addrs(endpoint.id().as_bytes());
	crate::info!("virtual IPv6: {v6}");
	crate::info!("virtual IPv4: {v4}");

	let tun_device = tun::create(v4, v6)?;
	crate::info!("tun device up: {}", tun::interface_name());

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

	// The resolver is not essential to the mesh: a failure here costs name
	// resolution, not traffic, so it warns rather than stopping the daemon.
	//
	// Registration happens once at startup, not per network: the zone we claim
	// is all of `.quix`, which does not change as networks are created, joined
	// or left.
	let iface = tun::interface_name();
	let mut registered = false;

	match dns::bind(&state).await {
		Ok((sockets, candidates)) => {
			// Serving starts first: the reachability check is answered by these
			// very sockets, so nothing can be verified until they are live.
			tokio::spawn(dns::serve(state.clone(), sockets));

			match dns::reachable_server(&candidates).await {
				Some(server) => match resolv::register(&iface, server).await {
					Ok(()) => {
						crate::info!(
							"registered *.{} with the system resolver via {server}",
							dns::ZONE
						);
						registered = true;
					}
					Err(e) => crate::warn!(
						"warning: could not register *.{} with the system resolver: {e:#}\n\
						 names still resolve via {}",
						dns::ZONE,
						fallback_resolver()
					),
				},
				// Nothing bound at all — `bind` has already said why for each
				// address, so repeating it here would only bury that.
				None if candidates.is_empty() => crate::warn!(
					"warning: no mesh address could be bound, so *.{} was not registered with \
					 the system resolver\n\
					 names still resolve via {}",
					dns::ZONE,
					fallback_resolver()
				),
				// Bound, but unreachable. Registering such an address is worse
				// than registering nothing: every lookup times out instead of
				// failing, and the daemon reports success while doing it.
				None => crate::warn!(
					"warning: nothing on this machine can reach the {} resolver, so it was not \
					 registered with the system resolver\n\
					 another VPN's IPv6 leak protection unbinds IPv6 from every adapter, and its \
					 DNS leak protection filters port {}; either will do this\n\
					 names still resolve via {}",
					dns::ZONE,
					dns::ZONE_PORT,
					fallback_resolver()
				),
			}
		}
		Err(e) => crate::warn!("warning: resolver did not start: {e:#}"),
	}

	tokio::spawn(mesh::tun_to_mesh(state.clone()));
	tokio::spawn(mesh::dialer(state.clone(), dial_rx));

	let result = tokio::select! {
		served = ipc::serve(state) => served,
		() = shutdown => {
			crate::info!("stopping");
			Ok(())
		}
	};

	// Hand the zone back before going away. On Linux the per-link settings would
	// vanish with the interface anyway; on Windows the NRPT rule is in the
	// registry and would outlive us.
	if registered {
		if let Err(e) = resolv::deregister(&iface).await {
			crate::warn!("warning: could not release *.{}: {e:#}", dns::ZONE);
		}
	}

	router.shutdown().await?;
	result
}
