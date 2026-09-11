use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use iroh::endpoint::Connection;
use iroh::EndpointId;
use tokio::sync::mpsc;

use crate::state::State;
use crate::ALPN;

/// How often the dialer sweeps for members it hasn't reached yet. Dials are
/// also triggered on demand, so this is just the backstop for peers that were
/// offline or unreachable at the time.
const REDIAL_INTERVAL: Duration = Duration::from_secs(15);

const CLOSE_NOT_MEMBER: u32 = 1;
const CLOSE_DUPLICATE: u32 = 2;

/// Runs one peer link for as long as it lives: registers it in the routing
/// table, pumps inbound datagrams into the TUN, and unregisters on the way out.
///
/// Both the accept side and the dial side end up here, so a link behaves the
/// same however it was established.
pub async fn serve_link(state: State, conn: Connection) {
	let peer = conn.remote_id();

	if !state.is_member(&peer.to_string()).await {
		println!("rejected link from non-member: {peer}");
		conn.close(CLOSE_NOT_MEMBER.into(), b"not a member");
		return;
	}

	if !state.peers().register(peer, conn.clone()).await {
		// Both ends dialed at the same time; the link already in the table wins.
		conn.close(CLOSE_DUPLICATE.into(), b"duplicate link");
		return;
	}

	println!("peer linked: {peer}");

	// Checked once here rather than per packet: if this ever fires, small
	// packets will flow and large ones will vanish, so say so loudly.
	match conn.max_datagram_size() {
		Some(max) if (crate::tun::MTU as usize) > max => eprintln!(
			"warning: link to {peer} carries {max}-byte datagrams but the MTU is {}; \
			 packets over {max} bytes will be dropped",
			crate::tun::MTU
		),
		None => eprintln!("warning: link to {peer} does not support datagrams; no traffic will flow"),
		_ => {}
	}

	// A member that was offline while others joined has a stale roster and
	// would reject them. Re-sync it now that we can reach them again.
	if state.is_coordinator().await {
		let state = state.clone();
		tokio::spawn(async move {
			let network_name = state.network_name().await;
			let roster = state.roster().await;
			if let Err(e) =
				crate::connect::push_roster(state.endpoint(), peer, network_name, roster).await
			{
				eprintln!("roster catch-up for {peer} failed: {e}");
			}
		});
	}

	let tun = state.tun.clone();

	loop {
		match conn.read_datagram().await {
			Ok(packet) => {
				state.stats().mesh_rx();
				if let Err(e) = tun.send(&packet).await {
					state.stats().tun_tx_err();
					eprintln!("tun write failed: {e}");
					break;
				}
				state.stats().tun_tx();
			}
			Err(e) => {
				println!("link to {peer} closed: {e}");
				break;
			}
		}
	}

	state.peers().unregister(&peer).await;
	println!("peer unlinked: {peer}");
}

/// The single reader on the TUN device: every outbound packet is looked up by
/// destination address and handed to that peer's connection.
///
/// There is exactly one of these. Reading the device from each connection task
/// instead would have them racing for packets and forwarding them to whichever
/// peer happened to win, which is not routing.
pub async fn tun_to_mesh(state: State) {
	let tun = state.tun.clone();
	// Headroom over the MTU: a short read would silently truncate a packet,
	// which is worse than the wasted bytes.
	let mut buf = vec![0u8; 2048];

	loop {
		let len = match tun.recv(&mut buf).await {
			Ok(len) => len,
			Err(e) => {
				eprintln!("tun read failed: {e}");
				return;
			}
		};

		let packet = &buf[..len];
		let Some(dst) = crate::tun::dst_ipv4(packet) else {
			continue; // not IPv4; nothing to route on
		};
		state.stats().tun_rx();

		let Some(peer) = state.peers().route(dst).await else {
			state.stats().no_route();
			continue;
		};
		let Some(conn) = state.peers().link(&peer).await else {
			// Routed but not connected yet: drop this packet and get a link up.
			state.stats().no_link();
			state.request_dial(peer);
			continue;
		};

		// Oversized packets are dropped without logging: this is the hot path,
		// and the condition is reported once per link in `serve_link` instead.
		if conn.max_datagram_size().is_some_and(|max| packet.len() > max) {
			state.stats().oversize();
			continue;
		}

		match conn.send_datagram(Bytes::copy_from_slice(packet)) {
			Ok(()) => state.stats().mesh_tx(),
			Err(e) => {
				state.stats().send_err();
				eprintln!("send to {peer} failed: {e}");
			}
		}
	}
}

/// Keeps the mesh connected: dials members we have a route to but no link.
pub async fn dialer(state: State, mut requests: mpsc::Receiver<EndpointId>) {
	let in_flight: Arc<Mutex<HashSet<EndpointId>>> = Arc::new(Mutex::new(HashSet::new()));
	let mut sweep = tokio::time::interval(REDIAL_INTERVAL);

	loop {
		let targets = tokio::select! {
			_ = sweep.tick() => state.peers().unlinked().await,
			request = requests.recv() => match request {
				Some(id) => vec![id],
				None => return,
			},
		};

		for id in targets {
			// Exactly one side of each pair dials. Without this both dial at
			// once, each rejects the other's connection as a duplicate, and
			// both tear down before reconnecting — a visible flap on every
			// startup. Comparing ids is a rule both ends compute identically.
			if state.own_id() > id {
				continue;
			}
			if state.peers().link(&id).await.is_some() {
				continue;
			}
			if !in_flight.lock().unwrap().insert(id) {
				continue;
			}

			let state = state.clone();
			let in_flight = in_flight.clone();
			tokio::spawn(async move {
				match state.endpoint().connect(id, ALPN).await {
					Ok(conn) => {
						in_flight.lock().unwrap().remove(&id);
						serve_link(state, conn).await;
					}
					Err(e) => {
						in_flight.lock().unwrap().remove(&id);
						eprintln!("dial to {id} failed: {e}");
					}
				}
			});
		}
	}
}
