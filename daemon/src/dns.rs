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
pub const ZONE_PORT: u16 = 53;
#[cfg(not(windows))]
pub const ZONE_PORT: u16 = 5354;

/// A freshly assigned IPv6 address is tentative until duplicate address
/// detection finishes, and binding it before then fails. Worth a few retries.
const BIND_ATTEMPTS: u32 = 10;
const BIND_RETRY: Duration = Duration::from_millis(300);

/// How long a candidate server has to answer its own probe. The listener is on
/// this machine, so anything slower than this is not slowness but a drop.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// A just-routed address can lose the first datagram, which is not the same as
/// being unreachable.
const PROBE_ATTEMPTS: u32 = 3;

/// The name a probe asks for. Nothing needs to be in the roster: what is being
/// tested is whether the query arrives, and NXDOMAIN proves that just as well.
const PROBE_NAME: &str = "probe";

/// Lets a reply be paired with the probe that asked for it.
const PROBE_ID: u16 = 0x9117;

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
///
/// Returns the sockets to serve on, and the mesh addresses that bound, in the
/// order they should be offered to the OS resolver. Which of them is actually
/// handed over is [`reachable_server`]'s decision, not this one's — binding an
/// address says nothing about whether anything can send to it.
pub async fn bind(state: &State) -> Result<(Vec<UdpSocket>, Vec<SocketAddr>)> {
	let testing = listen_addr()?;
	let socket = UdpSocket::bind(testing)
		.await
		.with_context(|| format!("binding the {ZONE} resolver to {testing}"))?;
	crate::info!("resolver listening on {testing} for *.{ZONE}");

	let mut sockets = vec![socket];
	let (v4, v6) = state.virtual_addrs();
	let mut candidates = Vec::new();

	// IPv6 first: it is the primary family, so it is preferred when both work.
	for addr in [IpAddr::V6(v6), IpAddr::V4(v4)] {
		let addr = SocketAddr::new(addr, ZONE_PORT);
		match bind_with_retry(addr).await {
			Ok(socket) => {
				crate::info!("resolver listening on {addr} for *.{ZONE}");
				sockets.push(socket);
				candidates.push(addr);
			}
			Err(e) => crate::warn!("warning: resolver could not bind {addr}: {e:#}"),
		}
	}

	Ok((sockets, candidates))
}

/// Picks the address to hand the OS resolver: the first candidate that answers.
///
/// Must be called once the sockets are being served, since it is those very
/// sockets that answer the probe.
pub async fn reachable_server(candidates: &[SocketAddr]) -> Option<SocketAddr> {
	for &addr in candidates {
		if probe(addr).await {
			return Some(addr);
		}
		crate::warn!("warning: the {ZONE} resolver is bound to {addr} but nothing can reach it there");
	}
	None
}

/// Whether a query sent to `server` actually comes back answered by us.
///
/// A successful bind is not evidence of this, which is what made the Windows
/// failure so quiet. An address can be assigned to an interface whose family is
/// switched off moments later — another VPN's IPv6 leak protection unbinds IPv6
/// on every adapter on the machine, ours included — leaving a bound socket on an
/// address that no longer exists. The port can equally be filtered by that same
/// VPN's DNS leak protection. Both look like a healthy listener from the inside,
/// and both used to be registered with the OS regardless, so every lookup timed
/// out while the daemon reported success.
///
/// Any reply of ours counts, NXDOMAIN included: the question is whether packets
/// arrive, not what the roster holds.
async fn probe(server: SocketAddr) -> bool {
	let Some(query) = probe_query() else {
		return false;
	};

	for _ in 0..PROBE_ATTEMPTS {
		if probe_once(server, &query).await {
			return true;
		}
	}
	false
}

async fn probe_once(server: SocketAddr, query: &[u8]) -> bool {
	// Same family as the target, or the send has nowhere to go from.
	let local: SocketAddr = match server {
		SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
		SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
	};

	let Ok(socket) = UdpSocket::bind(local).await else {
		return false;
	};
	if socket.send_to(query, server).await.is_err() {
		return false;
	}

	let mut buf = [0u8; MAX_PACKET];
	let Ok(Ok((len, _))) = tokio::time::timeout(PROBE_TIMEOUT, socket.recv_from(&mut buf)).await
	else {
		return false;
	};

	// The id pairs the reply with our query; the authority bit is what says the
	// answer came from us rather than from some other resolver that happens to
	// hold that address and port.
	Packet::parse(&buf[..len])
		.map(|reply| {
			reply.id() == PROBE_ID
				&& reply.has_flags(simple_dns::PacketFlag::AUTHORITATIVE_ANSWER)
		})
		.unwrap_or(false)
}

fn probe_query() -> Option<Vec<u8>> {
	// Owned, or the name would borrow a temporary that dies before the packet
	// is built.
	let name = format!("{PROBE_NAME}.{ZONE}");
	let mut packet = Packet::new_query(PROBE_ID);
	packet.questions.push(simple_dns::Question::new(
		simple_dns::Name::new(&name).ok()?.into_owned(),
		QTYPE::TYPE(simple_dns::TYPE::AAAA),
		simple_dns::QCLASS::CLASS(CLASS::IN),
		false,
	));
	packet.build_bytes_vec().ok()
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
				crate::warn!("resolver read failed: {e}");
				continue;
			}
		};

		// Resolving needs the roster, so snapshot it once per query.
		let peers = resolvable(&state).await;
		let network = state.network_name().await.map(|n| names::network_label(&n));
		let Some(reply) = answer(&buf[..len], &peers, network.as_deref()) else {
			// Dropping it silently makes a client report a timeout, which is
			// indistinguishable from the packet never arriving — the one thing
			// worth knowing here is that it did.
			crate::warn!("resolver could not parse a {len}-byte query from {from}");
			continue;
		};
		if let Err(e) = socket.send_to(&reply, from).await {
			crate::warn!("resolver reply to {from} failed: {e}");
		}
	}
}

/// One name this node answers for.
type Entry = (String, Ipv4Addr, Ipv6Addr);

/// Builds the reply to one query, or None if the request was not a DNS message.
fn answer(request: &[u8], peers: &[Entry], network: Option<&str>) -> Option<Vec<u8>> {
	let query = Packet::parse(request).ok()?;
	let mut reply = Packet::new_reply(query.id());

	// We are the authority for this zone, so say so rather than looking like a
	// cache that happens to have the record.
	reply.set_flags(simple_dns::PacketFlag::AUTHORITATIVE_ANSWER);
	// Convention is to reflect the client's recursion-desired bit back.
	if query.has_flags(simple_dns::PacketFlag::RECURSION_DESIRED) {
		reply.set_flags(simple_dns::PacketFlag::RECURSION_DESIRED);
	}

	let mut found = false;
	for question in &query.questions {
		// A reply must echo the question it answers: resolvers match the two to
		// pair a response with its request, and reject a reply that does not.
		// dig tolerates the omission and still prints the answer, which makes
		// this easy to miss.
		reply.questions.push(question.clone());

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
			question.qname.clone(),
			CLASS::IN,
			TTL,
			rdata,
		));
		found = true;
	}

	if !found {
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
	fn a_reply_echoes_the_question_it_answers() {
		// Without this a resolver cannot pair the reply with its request and
		// discards it. systemd-resolved reports "Received invalid reply"; dig
		// prints the answer regardless, so only a real resolver catches it.
		reply_to("nas.quix", simple_dns::TYPE::AAAA, |reply| {
			assert_eq!(reply.questions.len(), 1);
			assert_eq!(reply.questions[0].qname.to_string(), "nas.quix");
		});
	}

	#[test]
	fn a_reply_claims_authority_for_the_zone() {
		reply_to("nas.quix", simple_dns::TYPE::AAAA, |reply| {
			assert!(reply.has_flags(simple_dns::PacketFlag::AUTHORITATIVE_ANSWER));
		});
	}

	#[test]
	fn even_an_nxdomain_echoes_the_question() {
		reply_to("nope.quix", simple_dns::TYPE::AAAA, |reply| {
			assert_eq!(reply.rcode(), RCODE::NameError);
			assert_eq!(reply.questions.len(), 1, "still has to be matchable");
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
	fn a_query_carrying_edns_is_still_answered() {
		// Windows nslookup and most modern clients advertise EDNS0. If the OPT
		// record made the query unparseable we would silently answer nothing,
		// which the client reports as a timeout rather than an error.
		let mut packet = Packet::new_query(1);
		packet.questions.push(simple_dns::Question::new(
			simple_dns::Name::new("nas.homelab.quix").unwrap(),
			QTYPE::TYPE(simple_dns::TYPE::AAAA),
			simple_dns::QCLASS::CLASS(CLASS::IN),
			false,
		));
		packet.set_flags(simple_dns::PacketFlag::RECURSION_DESIRED);
		packet.opt_mut().replace(simple_dns::rdata::OPT {
			udp_packet_size: 4096,
			version: 0,
			opt_codes: Default::default(),
		});

		let bytes = packet.build_bytes_vec().unwrap();
		let reply = answer(&bytes, &peers(), Some("homelab"));
		assert!(reply.is_some(), "an EDNS query must still get a reply");
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

	/// Serves exactly one query on an ephemeral loopback port, so a real DNS
	/// client can be pointed at it.
	///
	/// Round-tripping our own output through our own parser cannot catch a
	/// reply that is well-formed but unusable — an empty question section, for
	/// instance, which `dig` prints happily and a resolver discards.
	#[tokio::test]
	async fn a_real_dns_client_accepts_our_reply() {
		let Ok(dig) = which_dig() else {
			crate::warn!("skipping: dig is not installed");
			return;
		};

		let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let port = socket.local_addr().unwrap().port();

		tokio::spawn(async move {
			let mut buf = vec![0u8; MAX_PACKET];
			let (len, from) = socket.recv_from(&mut buf).await.unwrap();
			let reply = answer(&buf[..len], &peers(), Some("homelab")).unwrap();
			socket.send_to(&reply, from).await.unwrap();
		});

		let out = tokio::process::Command::new(dig)
			.args([
				"@127.0.0.1",
				"-p",
				&port.to_string(),
				"nas.homelab.quix",
				"AAAA",
			])
			.output()
			.await
			.unwrap();
		let text = String::from_utf8_lossy(&out.stdout);

		assert!(text.contains("status: NOERROR"), "{text}");
		// The question has to come back, or a resolver cannot match the reply.
		assert!(text.contains("QUERY: 1"), "question not echoed:\n{text}");
		assert!(text.contains("flags: qr aa"), "not authoritative:\n{text}");
		assert!(text.contains("200::1"), "wrong address:\n{text}");
	}

	fn which_dig() -> Result<String> {
		let out = std::process::Command::new("sh")
			.args(["-c", "command -v dig"])
			.output()?;
		match out.status.success() {
			true => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
			false => anyhow::bail!("no dig"),
		}
	}

	/// Answers on an ephemeral loopback port exactly as the real listeners do,
	/// so a probe can be pointed at something live.
	async fn live_listener() -> SocketAddr {
		let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		let addr = socket.local_addr().unwrap();

		tokio::spawn(async move {
			let mut buf = vec![0u8; MAX_PACKET];
			while let Ok((len, from)) = socket.recv_from(&mut buf).await {
				if let Some(reply) = answer(&buf[..len], &peers(), Some("homelab")) {
					let _ = socket.send_to(&reply, from).await;
				}
			}
		});
		addr
	}

	/// An address with nothing behind it: bound long enough to get a port the
	/// OS is not otherwise using, then released.
	async fn dead_address() -> SocketAddr {
		let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		socket.local_addr().unwrap()
	}

	#[tokio::test]
	async fn a_listener_that_answers_is_reachable() {
		assert!(probe(live_listener().await).await);
	}

	#[tokio::test]
	async fn an_address_that_answers_nothing_is_not_reachable() {
		assert!(!probe(dead_address().await).await);
	}

	#[tokio::test]
	async fn a_bound_address_nothing_can_reach_is_never_registered() {
		// The Windows failure this exists for. The socket bound, so the daemon
		// took the address for usable and pointed NRPT at it, while every query
		// sent there was dropped — by an interface whose IPv6 had been switched
		// off underneath us, or by another VPN filtering port 53. Binding proves
		// nothing; answering does.
		assert_eq!(reachable_server(&[dead_address().await]).await, None);
	}

	#[tokio::test]
	async fn an_unreachable_candidate_is_skipped_for_one_that_works() {
		// IPv6 is offered first, so a dead IPv6 address used to be registered in
		// preference to a working IPv4 one. It must now fall through instead.
		let dead = dead_address().await;
		let live = live_listener().await;

		assert_eq!(reachable_server(&[dead, live]).await, Some(live));
	}

	#[tokio::test]
	async fn the_preferred_candidate_wins_when_both_answer() {
		let first = live_listener().await;
		let second = live_listener().await;

		assert_eq!(reachable_server(&[first, second]).await, Some(first));
	}

	#[test]
	fn a_probe_reply_carries_back_everything_the_probe_matches_on() {
		// `probe_once` accepts a reply only on its id and the authority bit, so
		// both have to survive the round trip through our own answer path. An id
		// that came back altered would make every probe fail, and nothing would
		// ever be registered with the OS resolver again.
		let query = probe_query().expect("a probe query");
		let bytes = answer(&query, &peers(), Some("homelab")).expect("a reply");
		let reply = Packet::parse(&bytes).expect("a well-formed reply");

		assert_eq!(reply.id(), PROBE_ID, "the id must come back unaltered");
		assert!(reply.has_flags(simple_dns::PacketFlag::AUTHORITATIVE_ANSWER));
		// NXDOMAIN is the expected answer and still proves the path works.
		assert_eq!(reply.rcode(), RCODE::NameError);
	}

	#[test]
	fn a_probe_is_a_query_this_resolver_would_answer() {
		// A probe asking something outside the zone would be met with NXDOMAIN
		// by anyone, which would make the check prove nothing.
		let bytes = probe_query().expect("a probe query");
		let query = Packet::parse(&bytes).unwrap();

		assert_eq!(query.id(), PROBE_ID);
		assert_eq!(
			host_in_zone(&query.questions[0].qname.to_string(), None),
			Some(PROBE_NAME.to_string())
		);
	}

	#[test]
	fn the_default_listen_address_is_loopback_and_not_port_53() {
		let addr: SocketAddr = DEFAULT_ADDR.parse().unwrap();
		assert!(addr.ip().is_loopback());
		assert_ne!(addr.port(), 53, "not fighting the system resolver yet");
	}
}
