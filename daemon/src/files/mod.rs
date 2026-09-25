//! Peer-to-peer file transfer, wormhole style.
//!
//! Nothing is stored. The sender's CLI stays connected from `quix file send`
//! until the receiver answers, and on acceptance the bytes run straight through
//! both daemons — file, sending CLI, sending daemon, network, receiving daemon,
//! receiving CLI, file — a bounded number of frames at a time. Neither daemon
//! writes file contents anywhere, so neither needs room for them, and a file
//! only ever touches disk under the permissions of the users at either end.
//!
//! Each offer is one `quix-file/0` connection holding one bidirectional stream
//! for its whole life, and **closing it means cancel**, from either end: a
//! sender pressing Ctrl+C, a CLI dying or a daemon restarting all withdraw the
//! offer on the other side without a message of their own. Transfers are not
//! resumable — one that fails partway fails on both sides and is sent again.

#[cfg(test)]
mod tests;
mod transfer;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::endpoint::{Connection, SendStream};
use iroh::{Endpoint, EndpointId};
use proto::frame::FrameReader;
use proto::{IncomingOffer, OfferState, OutgoingOffer, MAX_FILE_TTL, MAX_PENDING_PER_PEER};
use tokio::time::Instant;

pub use transfer::{respond, FileHandler};

pub const FILE_ALPN: &[u8] = b"quix-file/0";

/// How long a finished offer stays in `quix file list`, so whoever sent it can
/// still see what happened after their terminal has moved on.
const HISTORY: Duration = Duration::from_secs(60 * 60);

/// Clamped here, on both sides, whatever a client or a peer asked for: the
/// window is how long a stranger's name sits in someone's list, and how long a
/// sender's terminal is held.
pub fn clamp_ttl(ttl_secs: u64) -> Duration {
	Duration::from_secs(ttl_secs.clamp(1, MAX_FILE_TTL))
}

/// Offers in flight on this node, both directions. In memory only: an offer is
/// a live connection, and none survives the daemon restarting.
#[derive(Clone)]
pub struct Files {
	endpoint: Endpoint,
	registry: Arc<Mutex<Registry>>,
}

impl std::fmt::Debug for Files {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Files").finish_non_exhaustive()
	}
}

/// The streams of an offer waiting here, held until someone answers it.
pub(crate) struct Link {
	pub send: SendStream,
	pub recv: FrameReader,
}

struct Incoming {
	peer: EndpointId,
	name: String,
	size: u64,
	received_at: Instant,
	expires_at: Instant,
	conn: Connection,
	/// `None` for the moment between admitting an offer and telling the sender
	/// it was queued — reserved, so the per-peer limit counts it, but not yet
	/// something anyone can accept.
	link: Option<Link>,
}

struct Outgoing {
	peer: EndpointId,
	to: String,
	name: String,
	size: u64,
	state: OfferState,
	created_at: Instant,
	expires_at: Option<Instant>,
	finished_at: Option<Instant>,
	detail: Option<String>,
	/// While the offer is live, so removing the peer from the roster can end it.
	conn: Option<Connection>,
}

#[derive(Default)]
struct Registry {
	/// Current members other than us, with the name `status` shows for each.
	/// Nobody outside this may offer us anything or be offered anything.
	roster: HashMap<EndpointId, String>,
	incoming: HashMap<String, Incoming>,
	outgoing: HashMap<String, Outgoing>,
	/// Accepted transfers under way here, so a sender removed from the network
	/// mid-transfer is cut off rather than allowed to finish.
	receiving: HashMap<String, (EndpointId, Connection)>,
	/// Offers that ended without an answer, and why, so answering one late says
	/// what happened to it rather than that it never existed. Kept as long as
	/// finished outgoing offers are.
	gone: HashMap<String, (Instant, String)>,
}

/// Close codes on a `quix-file/0` connection. The reason bytes carry a message
/// for the other side's user; the code is for anything that wants to branch.
pub(crate) mod close {
	pub const DONE: u32 = 0;
	pub const CANCELLED: u32 = 1;
	pub const EXPIRED: u32 = 2;
	pub const NOT_MEMBER: u32 = 3;
	pub const FAILED: u32 = 4;
	pub const REJECTED: u32 = 5;
}

impl Files {
	pub fn new(endpoint: Endpoint) -> Self {
		Self {
			endpoint,
			registry: Arc::default(),
		}
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
		// Nothing here panics while holding the lock, but if something ever
		// does, the offers are still better served than lost.
		self.registry
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
	}

	/// Replaces who counts as a member, and ends every offer and transfer
	/// involving someone who no longer does. Called on every roster change,
	/// alongside the routing table.
	pub fn set_roster(&self, members: impl IntoIterator<Item = (EndpointId, String)>) {
		let mut registry = self.lock();
		registry.roster = members.into_iter().collect();
		let current: HashSet<EndpointId> = registry.roster.keys().copied().collect();

		registry.incoming.retain(|_, offer| {
			let keep = current.contains(&offer.peer);
			if !keep {
				offer.conn.close(
					close::NOT_MEMBER.into(),
					b"no longer a member of this network",
				);
			}
			keep
		});
		registry.receiving.retain(|_, (peer, conn)| {
			let keep = current.contains(peer);
			if !keep {
				conn.close(
					close::NOT_MEMBER.into(),
					b"no longer a member of this network",
				);
			}
			keep
		});
		// Outgoing entries stay listed; closing the connection makes the task
		// running the offer record it as failed, with the reason.
		for offer in registry.outgoing.values() {
			if let Some(conn) = offer
				.conn
				.as_ref()
				.filter(|_| !current.contains(&offer.peer))
			{
				conn.close(close::NOT_MEMBER.into(), b"removed from the network");
			}
		}
	}

	pub fn is_member(&self, peer: &EndpointId) -> bool {
		self.lock().roster.contains_key(peer)
	}

	fn display(&self, peer: &EndpointId) -> String {
		self.lock()
			.roster
			.get(peer)
			.cloned()
			.unwrap_or_else(|| crate::names::fallback(&peer.to_string()))
	}

	/// What `quix file list` shows: offers waiting here, oldest first, and this
	/// node's own offers, recent history included.
	pub fn list(&self) -> (Vec<IncomingOffer>, Vec<OutgoingOffer>) {
		let now = Instant::now();
		let mut registry = self.lock();
		registry.forget_history(now);

		let mut incoming: Vec<(&String, &Incoming)> = registry
			.incoming
			.iter()
			.filter(|(_, offer)| offer.link.is_some() && offer.expires_at > now)
			.collect();
		incoming.sort_by_key(|(_, offer)| offer.received_at);
		let incoming = incoming
			.into_iter()
			.map(|(id, offer)| IncomingOffer {
				id: id.clone(),
				from: registry
					.roster
					.get(&offer.peer)
					.cloned()
					.unwrap_or_else(|| crate::names::fallback(&offer.peer.to_string())),
				name: offer.name.clone(),
				size: offer.size,
				expires_in_secs: remaining(offer.expires_at, now),
			})
			.collect();

		let mut outgoing: Vec<(&String, &Outgoing)> = registry.outgoing.iter().collect();
		outgoing.sort_by_key(|(_, offer)| offer.created_at);
		let outgoing = outgoing
			.into_iter()
			.map(|(id, offer)| OutgoingOffer {
				id: id.clone(),
				to: offer.to.clone(),
				name: offer.name.clone(),
				size: offer.size,
				state: offer.state,
				expires_in_secs: offer.expires_at.map(|at| remaining(at, now)),
				detail: offer.detail.clone(),
			})
			.collect();

		(incoming, outgoing)
	}
}

impl Registry {
	/// A fresh id, unique among everything this node is tracking. Ours alone:
	/// a peer never gets to pick the id we show for its offer.
	fn new_id(&self) -> String {
		loop {
			let mut bytes = [0u8; 4];
			getrandom::fill(&mut bytes).expect("failed to get random bytes");
			let id = hex::encode(bytes);
			if !self.incoming.contains_key(&id)
				&& !self.outgoing.contains_key(&id)
				&& !self.gone.contains_key(&id)
			{
				return id;
			}
		}
	}

	/// Reserves a place for an offer from `peer`, or says why there is none.
	///
	/// Reserving and counting happen under one lock, so a peer firing offers
	/// concurrently cannot squeeze past the limit between the two.
	fn admit(
		&mut self,
		peer: EndpointId,
		name: String,
		size: u64,
		ttl: Duration,
		conn: Connection,
		now: Instant,
	) -> Result<String, String> {
		if !self.roster.contains_key(&peer) {
			return Err("not a member of this network".to_string());
		}
		let waiting = self
			.incoming
			.values()
			.filter(|offer| offer.peer == peer)
			.count();
		if waiting >= MAX_PENDING_PER_PEER {
			return Err(format!(
				"{MAX_PENDING_PER_PEER} of your offers are already waiting there"
			));
		}

		let id = self.new_id();
		self.incoming.insert(
			id.clone(),
			Incoming {
				peer,
				name,
				size,
				received_at: now,
				expires_at: now + ttl,
				conn,
				link: None,
			},
		);
		Ok(id)
	}

	/// Drops finished outgoing entries, and the record of incoming offers that
	/// went unanswered, once they have been around long enough.
	fn forget_history(&mut self, now: Instant) {
		self.outgoing.retain(|_, offer| match offer.finished_at {
			Some(at) => now.saturating_duration_since(at) < HISTORY,
			None => true,
		});
		self.gone
			.retain(|_, (at, _)| now.saturating_duration_since(*at) < HISTORY);
	}

	/// Moves an outgoing offer to a new state, stamping when it finished.
	fn set_state(&mut self, id: &str, state: OfferState, detail: Option<String>, now: Instant) {
		if let Some(offer) = self.outgoing.get_mut(id) {
			offer.state = state;
			if state != OfferState::Offered {
				offer.expires_at = None;
			}
			if state.is_finished() {
				offer.finished_at = Some(now);
				offer.detail = detail;
				offer.conn = None;
			}
		}
	}
}

fn remaining(deadline: Instant, now: Instant) -> u64 {
	deadline.saturating_duration_since(now).as_secs()
}

#[cfg(test)]
mod registry_tests {
	//! The rules that need no network: limits, windows, history. The clock is a
	//! parameter throughout, so none of this depends on how fast tests run.

	use super::*;

	fn peer(n: u8) -> EndpointId {
		iroh::SecretKey::from_bytes(&[n; 32]).public()
	}

	#[test]
	fn a_window_is_clamped_to_a_day_and_never_to_nothing() {
		assert_eq!(clamp_ttl(0), Duration::from_secs(1));
		assert_eq!(clamp_ttl(600), Duration::from_secs(600));
		assert_eq!(clamp_ttl(MAX_FILE_TTL), Duration::from_secs(MAX_FILE_TTL));
		assert_eq!(
			clamp_ttl(MAX_FILE_TTL + 1),
			Duration::from_secs(MAX_FILE_TTL)
		);
		assert_eq!(
			clamp_ttl(u64::MAX),
			Duration::from_secs(MAX_FILE_TTL),
			"no overflow"
		);
	}

	#[test]
	fn the_default_window_is_ten_minutes_and_the_cap_a_day() {
		assert_eq!(proto::DEFAULT_FILE_TTL, 10 * 60);
		assert_eq!(MAX_FILE_TTL, 24 * 60 * 60);
		assert_eq!(MAX_PENDING_PER_PEER, 20);
	}

	#[test]
	fn a_finished_offer_is_listed_for_an_hour_and_then_forgotten() {
		let start = Instant::now();
		let mut registry = Registry::default();
		registry.outgoing.insert(
			"0000aaaa".to_string(),
			Outgoing {
				peer: peer(2),
				to: "nas".to_string(),
				name: "a.txt".to_string(),
				size: 1,
				state: OfferState::Offered,
				created_at: start,
				expires_at: Some(start + Duration::from_secs(600)),
				finished_at: None,
				detail: None,
				conn: None,
			},
		);

		registry.forget_history(start + Duration::from_secs(7200));
		assert_eq!(
			registry.outgoing.len(),
			1,
			"a live offer is never forgotten"
		);

		registry.set_state("0000aaaa", OfferState::Done, None, start);
		assert_eq!(
			registry.outgoing["0000aaaa"].expires_at, None,
			"nothing left to expire"
		);

		registry.forget_history(start + HISTORY - Duration::from_secs(1));
		assert_eq!(registry.outgoing.len(), 1, "still within the hour");

		registry.forget_history(start + HISTORY);
		assert!(registry.outgoing.is_empty(), "gone after the hour");
	}

	#[test]
	fn ids_are_eight_hex_characters_and_unique() {
		let registry = Registry::default();
		let id = registry.new_id();
		assert_eq!(id.len(), 8);
		assert!(
			id.chars()
				.all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
			"{id}"
		);
	}
}
