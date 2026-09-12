use anyhow::{Context, Result};
use iroh::endpoint::PathId;
use iroh::{Endpoint, EndpointId};
use std::time::Duration;

use crate::admin::{AdminRequest, AdminResponse, ADMIN_ALPN};
use crate::membership::{Member, Membership};
use crate::state::State;

/// What a join returned: the network's name and the roster to start from.
pub struct Admission {
	pub coordinator_id: EndpointId,
	pub network_name: Option<String>,
	pub members: Vec<Member>,
	/// The hostname the coordinator assigned, if one was requested.
	pub hostname: Option<String>,
}

/// Result of probing a peer over the mesh.
pub struct Probe {
	pub v4: std::net::Ipv4Addr,
	pub v6: std::net::Ipv6Addr,
	pub rtt: Option<Duration>,
}

/// How long `probe` waits for the dialer to bring a link up before giving up.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Attempts to reach the coordinator during a join, and the gap between them.
const JOIN_ATTEMPTS: u32 = 3;
const JOIN_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Probes a peer's data-plane link, asking the dialer for one if it isn't up
/// yet. Reports the RTT QUIC measures on the live connection.
///
/// This deliberately goes through the dialer rather than connecting itself, so
/// the link it reports on is the one the mesh actually forwards over.
pub async fn probe(state: &State, peer: &str) -> Result<Probe> {
	let id: EndpointId = peer.parse().context("parse peer id")?;

	if !state.is_member(&id.to_string()).await {
		anyhow::bail!("{id} is not a member of this network");
	}

	let conn = match state.peers().link(&id).await {
		Some(conn) => conn,
		None => {
			state.request_dial(id);
			wait_for_link(state, id)
				.await
				.context("no link to peer — it may be offline")?
		}
	};

	let (v4, v6) = crate::tun::virtual_addrs(id.as_bytes());
	Ok(Probe {
		v4,
		v6,
		rtt: conn.rtt(PathId::ZERO),
	})
}

async fn wait_for_link(state: &State, id: EndpointId) -> Option<iroh::endpoint::Connection> {
	tokio::time::timeout(PROBE_TIMEOUT, async {
		loop {
			if let Some(conn) = state.peers().link(&id).await {
				return conn;
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	})
	.await
	.ok()
}

/// Redeems an invite code with the coordinator it names.
///
/// Reaching the coordinator is retried: a coordinator that just came up may not
/// have finished publishing its address, and resolving it is the one step of a
/// join that depends on something outside both peers.
pub async fn join_network(
	endpoint: &Endpoint,
	code: &str,
	hostname: Option<String>,
) -> Result<Admission> {
	let (coordinator_id, token) = Membership::decode_invite(code)?;

	let mut last_error = None;
	let mut response = None;

	for attempt in 0..JOIN_ATTEMPTS {
		if attempt > 0 {
			tokio::time::sleep(JOIN_RETRY_DELAY).await;
		}

		let req = AdminRequest::Join {
			token_hex: hex::encode(token),
			hostname: hostname.clone(),
		};

		match admin_call(endpoint, coordinator_id, req).await {
			Ok(resp) => {
				response = Some(resp);
				break;
			}
			Err(e) => last_error = Some(e),
		}
	}

	let response = match response {
		Some(resp) => resp,
		None => {
			return Err(last_error
				.unwrap_or_else(|| anyhow::anyhow!("could not reach the coordinator"))
				.context("could not reach the coordinator — is it online?"))
		}
	};

	match response {
		AdminResponse::Joined {
			network_name,
			members,
			hostname,
		} => Ok(Admission {
			coordinator_id,
			network_name,
			members,
			hostname,
		}),
		AdminResponse::Error { message } => anyhow::bail!(message),
		other => anyhow::bail!("unexpected response to join: {other:?}"),
	}
}

/// Member → coordinator: ask to be dropped from the roster, so the rest of the
/// network stops dialing us.
pub async fn notify_leave(endpoint: &Endpoint, coordinator: EndpointId) -> Result<()> {
	match admin_call(endpoint, coordinator, AdminRequest::Leave).await? {
		AdminResponse::Ack => Ok(()),
		AdminResponse::Error { message } => anyhow::bail!(message),
		other => anyhow::bail!("unexpected response to leave: {other:?}"),
	}
}

/// Coordinator → member: publish an updated roster.
pub async fn push_roster(
	endpoint: &Endpoint,
	to: EndpointId,
	network_name: Option<String>,
	members: Vec<Member>,
) -> Result<()> {
	let req = AdminRequest::Roster {
		network_name,
		members,
	};

	match admin_call(endpoint, to, req).await? {
		AdminResponse::Ack => Ok(()),
		AdminResponse::Error { message } => anyhow::bail!(message),
		other => anyhow::bail!("unexpected response to roster push: {other:?}"),
	}
}

/// Member → coordinator: claim a hostname, returning the name assigned, which
/// may carry a suffix if the requested one was taken.
pub async fn claim_hostname(
	endpoint: &Endpoint,
	coordinator: EndpointId,
	hostname: String,
) -> Result<String> {
	let req = AdminRequest::SetHostname { hostname };

	match admin_call(endpoint, coordinator, req).await? {
		AdminResponse::HostnameSet { hostname } => Ok(hostname),
		AdminResponse::Error { message } => anyhow::bail!(message),
		other => anyhow::bail!("unexpected response to hostname claim: {other:?}"),
	}
}

/// One admin request/response exchange on its own stream.
async fn admin_call(
	endpoint: &Endpoint,
	to: EndpointId,
	req: AdminRequest,
) -> Result<AdminResponse> {
	let conn = endpoint.connect(to, ADMIN_ALPN).await.context("connect")?;
	let (mut send, mut recv) = conn.open_bi().await.context("open stream")?;

	send.write_all(&serde_json::to_vec(&req)?)
		.await
		.context("write")?;
	send.finish().context("finish")?;

	let data = recv.read_to_end(64 * 1024).await.context("read response")?;
	conn.close(0u32.into(), b"done");

	serde_json::from_slice(&data).context("parse admin response")
}
