mod admin;
mod args;
mod authz;
mod connect;
mod dns;
mod files;
mod handler;
mod identity;
mod ipc;
mod log;
mod membership;
mod mesh;
mod names;
mod peers;
mod registration;
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
	//
	// Asked before the command line is read, deliberately: the SCM passes the
	// service's own start parameters, which are not ours to interpret, and a
	// service that refused to start over one would be a poor trade for a
	// stricter terminal.
	#[cfg(windows)]
	if service::run() {
		return Ok(());
	}

	// Anything but a bare invocation is answered here and nothing is started.
	match args::parse(std::env::args().skip(1))? {
		// Not through `crate::info!`: this is a question asked at a terminal,
		// and the answer belongs on stdout rather than in the daemon's log.
		args::Invocation::Version => {
			println!("quixd {}", proto::VERSION_TAG);
			return Ok(());
		}
		args::Invocation::Help => {
			println!("{}", args::USAGE);
			return Ok(());
		}
		args::Invocation::Run => {}
	}

	runtime()?.block_on(async {
		// Installed before the daemon starts, so a stop that arrives while it is
		// still waiting for a relay is honoured rather than fatal.
		let shutdown = shutdown_signal();
		run(shutdown).await
	})
}

/// Resolves once the daemon has been asked to stop: Ctrl+C, and on Unix also
/// SIGTERM, which is how systemd stops a service.
///
/// The handlers are installed when this is called, not when the future is first
/// polled, so a signal arriving in between is held rather than killing the
/// process. Must be called from inside the runtime.
// Deliberately not an `async fn`, which would install nothing until first
// polled. Off Unix there is nothing to install up front, which is all clippy
// sees there.
#[cfg_attr(not(unix), allow(clippy::manual_async_fn))]
fn shutdown_signal() -> impl Future<Output = ()> {
	#[cfg(unix)]
	let terminate = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
		Ok(stream) => Some(stream),
		Err(e) => {
			// Not fatal: the daemon still runs, it just cannot shut down cleanly
			// under a service manager.
			crate::warn!("warning: could not handle SIGTERM ({e}); only Ctrl+C will stop the daemon cleanly");
			None
		}
	};

	async move {
		#[cfg(unix)]
		if let Some(mut terminate) = terminate {
			tokio::select! {
				_ = tokio::signal::ctrl_c() => crate::info!("interrupted"),
				_ = terminate.recv() => crate::info!("terminated"),
			}
			return;
		}

		let _ = tokio::signal::ctrl_c().await;
		crate::info!("interrupted");
	}
}

pub fn runtime() -> Result<tokio::runtime::Runtime> {
	Ok(tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()?)
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
		// Its own connections, never the data plane's: a transfer shares no
		// flow control with VPN traffic and cannot stall it.
		.accept(
			files::FILE_ALPN,
			files::FileHandler {
				files: state.files().clone(),
			},
		)
		.spawn();

	// The resolver is not essential to the mesh: a failure here costs name
	// resolution, not traffic, so it warns rather than stopping the daemon.
	//
	// Registration is for all of `.quix`, not per network: the zone we claim does
	// not change as networks are created, joined or left.
	let registration = match dns::bind(&state).await {
		Ok((sockets, candidates)) => {
			// Serving starts first: the reachability check is answered by these
			// very sockets, so nothing can be verified until they are live.
			tokio::spawn(dns::serve(state.clone(), sockets));
			Some(registration::start(state.clone(), tun::interface_name(), candidates).await)
		}
		// `status` already reports this as unavailable: nothing was ever tried.
		Err(e) => {
			crate::warn!("warning: resolver did not start: {e:#}");
			None
		}
	};

	tokio::spawn(mesh::tun_to_mesh(state.clone()));
	tokio::spawn(mesh::dialer(state.clone(), dial_rx));

	let result = tokio::select! {
		served = ipc::serve(state) => served,
		() = shutdown => {
			crate::info!("stopping");
			Ok(())
		}
	};

	// Stop any retry still trying to claim the zone, and hand it back before
	// going away.
	if let Some(registration) = registration {
		registration.stop().await;
	}

	router.shutdown().await?;
	result
}

#[cfg(all(test, unix))]
mod tests {
	use super::*;
	use std::time::Duration;

	#[tokio::test]
	async fn sigterm_asks_the_daemon_to_stop() {
		// systemd stops a service with SIGTERM. Unhandled, that kills the process
		// outright: peers get no close, and the resolver is never deregistered.
		let shutdown = shutdown_signal();

		// SAFETY: signalling our own process. The handler was installed by the
		// call above, so this reaches it instead of terminating the test binary.
		unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };

		assert!(
			tokio::time::timeout(Duration::from_secs(5), shutdown).await.is_ok(),
			"SIGTERM must resolve the shutdown future"
		);
	}
}
