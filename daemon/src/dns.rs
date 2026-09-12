//! A resolver for the `.quix` zone, answering from the in-memory roster.
//!
//! Listens in two places: the mesh addresses, which is where the OS resolver is
//! pointed, and a loopback port that stays available for testing without
//! involving the system resolver at all:
//!
//! ```text
//! dig @127.0.0.1 -p 5354 nas.homelab.quix AAAA
//! dig @127.0.0.1 -p 5354 nas.quix AAAA          # flat form, same answer
//! ```
//!
//! Both families are answered. The zone is IPv6-first like the mesh itself, so
//! serving only A records would point every name at the compatibility address.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use simple_dns::rdata::{RData, A, AAAA};
use simple_dns::{Packet, ResourceRecord, CLASS, QTYPE, RCODE};
use tokio::net::UdpSocket;

use crate::names;
use crate::state::State;

/// The suffix this resolver is authoritative for.
pub const ZONE: &str = "quix";

/// The testing endpoint, reachable regardless of whether OS registration
/// worked. Never port 53, so it cannot collide with the system resolver.
const DEFAULT_ADDR: &str = "127.0.0.1:5354";

/// Where the OS resolver is told to send `.quix` queries.
///
/// Windows NRPT rules carry no port field, so 53 is not optional there and the
/// service runs as LocalSystem which may bind it. systemd-resolved accepts a
/// port, so Linux uses an unprivileged one and the daemon needs no capability
/// to bind sockets at all.
#[cfg(windows)]
const ZONE_PORT: u16 = 53;
#[cfg(not(windows))]
const ZONE_PORT: u16 = 5354;

/// A freshly assigned IPv6 address is tentative until duplicate address
/// detection finishes, and binding it before then fails. Worth a few retries.
const BIND_ATTEMPTS: u32 = 10;
const BIND_RETRY: Duration = Duration::from_millis(300);

/// Names are only as stable as the roster, and a roster push can change them at
/// any moment, so resolvers should not hold on to an answer for long.
const TTL: u32 = 30;

/// A DNS message never exceeds this over UDP without EDNS.
const MAX_PACKET: usize = 512;

pub fn listen_addr() -> Result<SocketAddr> {
	let raw = std::env::var("QUIX_DNS_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
	raw.parse()
		.with_context(|| format!("QUIX_DNS_ADDR is not an address: {raw}"))
}

/// Binds everywhere the resolver should answer.
///
/// The loopback endpoint is required — without it there is no resolver at all.
/// The mesh addresses are best-effort: losing them costs OS integration, not
/// the ability to answer.
pub async fn bind(state: &State) -> Result<(Vec<UdpSocket>, Option<SocketAddr>)> {
	let testing = listen_addr()?;
	let socket = UdpSocket::bind(testing)
		.await
		.with_context(|| format!("binding the {ZONE} resolver to {testing}"))?;
	println!("resolver listening on {testing} for *.{ZONE}");

	let mut sockets = vec![socket];
	let (v4, v6) = state.virtual_addrs();
	let mut zone_server = None;

	// IPv6 first: it is the primary family, so it is what the OS is pointed at
	// when both are available.
	for addr in [IpAddr::V6(v6), IpAddr::V4(v4)] {
		let addr = SocketAddr::new(addr, ZONE_PORT);
		match bind_with_retry(addr).await {
			Ok(socket) => {
				println!("resolver listening on {addr} for *.{ZONE}");
				sockets.push(socket);
				zone_server.get_or_insert(addr);
			}
			Err(e) => eprintln!("warning: resolver could not bind {addr}: {e:#}"),
		}
	}

	Ok((sockets, zone_server))
}

/// Retries past the window where a just-assigned address is not yet usable.
async fn bind_with_retry(addr: SocketAddr) -> Result<UdpSocket> {
	let mut last = None;
	for _ in 0..BIND_ATTEMPTS {
		match UdpSocket::bind(addr).await {
			Ok(socket) => return Ok(socket),
			Err(e) => {
				last = Some(e);
				tokio::time::sleep(BIND_RETRY).await;
			}
		}
	}
	Err(last.expect("at least one attempt").into())
}

/// Answers on every bound socket until the daemon stops.
pub async fn serve(state: State, sockets: Vec<UdpSocket>) {
	let state = Arc::new(state);
	let mut tasks = Vec::new();

	for socket in sockets {
		let state = state.clone();
		tasks.push(tokio::spawn(async move { answer_loop(socket, state).await }));
	}

	for task in tasks {
		let _ = task.await;
	}
}

async fn answer_loop(socket: UdpSocket, state: Arc<State>) {
	let mut buf = vec![0u8; MAX_PACKET];
	loop {
		let (len, from) = match socket.recv_from(&mut buf).await {
			Ok(received) => received,
			Err(e) => {
				eprintln!("resolver read failed: {e}");
				continue;
			}
		};

		// Resolving needs the roster, so snapshot it once per query.
		let peers = resolvable(&state).await;
		let network = state.network_name().await.map(|n| names::network_label(&n));
		let Some(reply) = answer(&buf[..len], &peers, network.as_deref()) else {
			continue; // unparseable; nothing useful to reply with
		};
		if let Err(e) = socket.send_to(&reply, from).await {
			eprintln!("resolver reply to {from} failed: {e}");
		}
	}
}

/// One name this node answers for.
type Entry = (String, Ipv4Addr, Ipv6Addr);

/// Builds the reply to one query, or None if the request was not a DNS message.
fn answer(request: &[u8], peers: &[Entry], network: Option<&str>) -> Option<Vec<u8>> {
	let query = Packet::parse(request).ok()?;
	let mut reply = Packet::new_reply(query.id());

	let mut found = false;
	for question in query.questions {
		let Some(label) = host_in_zone(&question.qname.to_string(), network) else {
			continue;
		};
		let Some((v4, v6)) = peers.iter().find(|(name, _, _)| *name == label).map(|(_, v4, v6)| (*v4, *v6)) else {
			continue;
		};

		let rdata = match question.qtype {
			QTYPE::TYPE(simple_dns::TYPE::A) => RData::A(A::from(v4)),
			QTYPE::TYPE(simple_dns::TYPE::AAAA) => RData::AAAA(AAAA::from(v6)),
			// The name exists but not for this type: an empty NOERROR, which
			// is what tells a resolver to stop rather than try elsewhere.
			_ => {
				found = true;
				continue;
			}
		};

		reply.answers.push(ResourceRecord::new(
			question.qname.clone().into_owned(),
			CLASS::IN,
			TTL,
			rdata,
		));
		found = true;
	}

	if !found {
		reply.set_flags(simple_dns::PacketFlag::AUTHORITATIVE_ANSWER);
		*reply.rcode_mut() = RCODE::NameError;
	}

	reply.build_bytes_vec().ok()
}

/// Extracts the host label from a query, if it is one we should answer.
///
/// Two forms are accepted: `host.network.quix`, which only matches when the
/// network label is this node's, and the flat `host.quix`. Keeping the flat
/// form means a name stays typable without remembering which network a peer is
/// on, and it is what already worked before networks had a label.
fn host_in_zone(qname: &str, network: Option<&str>) -> Option<String> {
	let name = qname.trim_end_matches('.').to_ascii_lowercase();
	let inner = name.strip_suffix(&format!(".{ZONE}"))?;

	match inner.split_once('.') {
		// Qualified: the middle label has to name this network, and there must
		// be nothing deeper.
		Some((host, rest)) => {
			(!host.is_empty() && Some(rest) == network).then(|| host.to_string())
		}
		None => (!inner.is_empty()).then(|| inner.to_string()),
	}
}

/// Every name this node can answer for, with the addresses behind it.
///
/// Fallback identifiers resolve alongside hostnames rather than only in their
/// absence: a fallback is derived from the peer's key, so it stays correct even
/// while a hostname is disputed.
async fn resolvable(state: &State) -> Vec<Entry> {
	let hostnames = state.hostnames().await;
	let mut out = Vec::new();

	for (id, v4, v6) in state.all_addrs().await {
		out.push((names::fallback(&id), v4, v6));
		if let Some(hostname) = hostnames.get(&id) {
			out.push((hostname.clone(), v4, v6));
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	const NET: Option<&str> = Some("homelab");

	#[test]
	fn a_qualified_name_resolves_when_the_network_matches() {
		assert_eq!(host_in_zone("nas.homelab.quix", NET), Some("nas".to_string()));
		assert_eq!(host_in_zone("nas.homelab.quix.", NET), Some("nas".to_string()));
		// Queries arrive in whatever case the client used.
		assert_eq!(host_in_zone("NAS.HOMELAB.QUIX", NET), Some("nas".to_string()));
	}

	#[test]
	fn a_qualified_name_for_another_network_is_not_ours() {
		assert_eq!(host_in_zone("nas.gaming.quix", NET), None);
	}

	#[test]
	fn the_flat_form_still_resolves() {
		assert_eq!(host_in_zone("nas.quix", NET), Some("nas".to_string()));
		// And on a node that has not joined anything yet.
		assert_eq!(host_in_zone("nas.quix", None), Some("nas".to_string()));
	}

	#[test]
	fn anything_outside_the_zone_is_not() {
		assert_eq!(host_in_zone("example.com", NET), None);
		assert_eq!(host_in_zone("quix", NET), None);
		assert_eq!(host_in_zone("nas.quix.example.com", NET), None);
		assert_eq!(host_in_zone(".quix", NET), None);
	}

	#[test]
	fn nothing_deeper_than_host_and_network_is_answered() {
		assert_eq!(host_in_zone("a.nas.homelab.quix", NET), None);
	}

	fn peers() -> Vec<Entry> {
		vec![(
			"nas".to_string(),
			Ipv4Addr::new(10, 1, 2, 3),
			"200::1".parse().unwrap(),
		)]
	}

	fn query(name: &str, qtype: simple_dns::TYPE) -> Vec<u8> {
		let mut packet = Packet::new_query(1);
		packet.questions.push(simple_dns::Question::new(
			simple_dns::Name::new(name).unwrap(),
			QTYPE::TYPE(qtype),
			simple_dns::QCLASS::CLASS(CLASS::IN),
			false,
		));
		packet.build_bytes_vec().unwrap()
	}

	/// `Packet` borrows the buffer it parsed, so inspect it in place.
	fn reply_to(name: &str, qtype: simple_dns::TYPE, check: impl FnOnce(&Packet)) {
		let bytes = answer(&query(name, qtype), &peers(), Some("homelab")).expect("a reply");
		check(&Packet::parse(&bytes).expect("a well-formed reply"));
	}

	#[test]
	fn an_a_query_answers_with_the_compatibility_address() {
		reply_to("nas.quix", simple_dns::TYPE::A, |reply| {
			assert_eq!(reply.rcode(), RCODE::NoError);
			assert_eq!(reply.answers.len(), 1);
			assert!(matches!(reply.answers[0].rdata, RData::A(_)));
		});
	}

	#[test]
	fn an_aaaa_query_answers_with_the_primary_address() {
		reply_to("nas.quix", simple_dns::TYPE::AAAA, |reply| {
			assert_eq!(reply.rcode(), RCODE::NoError);
			assert_eq!(reply.answers.len(), 1);
			assert!(matches!(reply.answers[0].rdata, RData::AAAA(_)));
		});
	}

	#[test]
	fn an_unknown_name_in_the_zone_is_nxdomain() {
		reply_to("nope.quix", simple_dns::TYPE::AAAA, |reply| {
			assert_eq!(reply.rcode(), RCODE::NameError);
			assert!(reply.answers.is_empty());
		});
	}

	#[test]
	fn names_outside_the_zone_are_not_answered() {
		reply_to("example.com", simple_dns::TYPE::A, |reply| {
			assert_eq!(reply.rcode(), RCODE::NameError);
			assert!(reply.answers.is_empty());
		});
	}

	#[test]
	fn a_known_name_with_an_unsupported_type_is_empty_not_nxdomain() {
		// The name exists; saying NXDOMAIN would wrongly tell a resolver that
		// nothing by that name is there at all.
		reply_to("nas.quix", simple_dns::TYPE::MX, |reply| {
			assert_eq!(reply.rcode(), RCODE::NoError);
			assert!(reply.answers.is_empty());
		});
	}

	#[test]
	fn garbage_is_not_answered_at_all() {
		assert!(answer(b"not a dns packet", &peers(), Some("homelab")).is_none());
	}

	#[test]
	fn the_zone_port_is_privileged_only_where_it_has_to_be() {
		// Windows NRPT cannot express a port, so 53 is forced there. Linux can,
		// so it uses an unprivileged one and needs no bind capability.
		let privileged = ZONE_PORT < 1024;
		assert_eq!(
			privileged,
			cfg!(windows),
			"port {ZONE_PORT} is privileged on a platform that need not be"
		);
	}

	#[test]
	fn a_server_address_formats_the_way_resolvectl_expects() {
		// resolved wants a bare IPv4 and a bracketed IPv6 when a port is given.
		let v4 = SocketAddr::new(Ipv4Addr::new(10, 1, 2, 3).into(), ZONE_PORT);
		let v6 = SocketAddr::new("200::1".parse::<Ipv6Addr>().unwrap().into(), ZONE_PORT);

		assert_eq!(v4.to_string(), format!("10.1.2.3:{ZONE_PORT}"));
		assert_eq!(v6.to_string(), format!("[200::1]:{ZONE_PORT}"));
	}

	#[test]
	fn the_default_listen_address_is_loopback_and_not_port_53() {
		let addr: SocketAddr = DEFAULT_ADDR.parse().unwrap();
		assert!(addr.ip().is_loopback());
		assert_ne!(addr.port(), 53, "not fighting the system resolver yet");
	}
}
