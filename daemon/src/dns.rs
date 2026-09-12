//! A resolver for the `.quix` zone, answering from the in-memory roster.
//!
//! Scoped deliberately: it binds a loopback UDP port of its own rather than
//! fighting the system resolver for port 53, and nothing registers the zone
//! with the OS yet. Test it directly:
//!
//! ```text
//! dig @127.0.0.1 -p 5354 nas.quix AAAA
//! ```
//!
//! Both families are answered. The zone is IPv6-first like the mesh itself, so
//! serving only A records would point every name at the compatibility address.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Context, Result};
use simple_dns::rdata::{RData, A, AAAA};
use simple_dns::{Packet, ResourceRecord, CLASS, QTYPE, RCODE};
use tokio::net::UdpSocket;

use crate::names;
use crate::state::State;

/// The suffix this resolver is authoritative for.
pub const ZONE: &str = "quix";

/// Loopback-only by default: the zone is not registered with the OS, so
/// anything reaching us got here deliberately.
const DEFAULT_ADDR: &str = "127.0.0.1:5354";

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

pub async fn serve(state: State) -> Result<()> {
	let addr = listen_addr()?;
	let socket = UdpSocket::bind(addr)
		.await
		.with_context(|| format!("binding the {ZONE} resolver to {addr}"))?;

	println!("resolver listening on {addr} for *.{ZONE}");

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
		let Some(reply) = answer(&buf[..len], &peers) else {
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
fn answer(request: &[u8], peers: &[Entry]) -> Option<Vec<u8>> {
	let query = Packet::parse(request).ok()?;
	let mut reply = Packet::new_reply(query.id());

	let mut found = false;
	for question in query.questions {
		let Some(label) = label_in_zone(&question.qname.to_string()) else {
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

/// Strips the zone suffix, returning the single label in front of it.
///
/// Written as a suffix match rather than a fixed comparison so `name.network.
/// quix` can be added without reworking the query path, once a node can belong
/// to more than one network.
fn label_in_zone(qname: &str) -> Option<String> {
	let name = qname.trim_end_matches('.').to_ascii_lowercase();
	let label = name.strip_suffix(&format!(".{ZONE}"))?;

	// One label for now; anything deeper is not ours to answer.
	match label.contains('.') {
		true => None,
		false => Some(label.to_string()),
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

	#[test]
	fn a_single_label_under_the_zone_is_ours() {
		assert_eq!(label_in_zone("nas.quix"), Some("nas".to_string()));
		assert_eq!(label_in_zone("nas.quix."), Some("nas".to_string()));
		// Queries arrive in whatever case the client used.
		assert_eq!(label_in_zone("NAS.QUIX"), Some("nas".to_string()));
	}

	#[test]
	fn anything_outside_the_zone_is_not() {
		assert_eq!(label_in_zone("example.com"), None);
		assert_eq!(label_in_zone("quix"), None);
		assert_eq!(label_in_zone("nas.quix.example.com"), None);
	}

	#[test]
	fn deeper_names_are_left_for_when_networks_have_labels() {
		// `nas.homelab.quix` becomes meaningful once a node can belong to more
		// than one network; until then it is not something we can answer.
		assert_eq!(label_in_zone("nas.homelab.quix"), None);
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
		let bytes = answer(&query(name, qtype), &peers()).expect("a reply");
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
		assert!(answer(b"not a dns packet", &peers()).is_none());
	}

	#[test]
	fn the_default_listen_address_is_loopback_and_not_port_53() {
		let addr: SocketAddr = DEFAULT_ADDR.parse().unwrap();
		assert!(addr.ip().is_loopback());
		assert_ne!(addr.port(), 53, "not fighting the system resolver yet");
	}
}
