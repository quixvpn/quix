use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use iroh::endpoint::Connection;
use iroh::EndpointId;
use tokio::sync::RwLock;

use crate::tun::virtual_addrs;

/// Close code sent to peers when we leave their network.
const CLOSE_LEFT: u32 = 3;

/// The mesh's forwarding state: which overlay address belongs to which peer,
/// and which of those peers we currently hold a live connection to.
///
/// Routes come from the membership roster and exist whether or not the peer is
/// reachable; links come and go as connections are established and dropped.
/// A packet for a routed-but-unlinked peer is what triggers a dial.
///
/// Every member contributes two routes, one per address family, both pointing
/// at the same peer — so an IPv4-only application reaches the same node an
/// IPv6 one does.
#[derive(Clone, Default)]
pub struct Peers {
	inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
	links: HashMap<EndpointId, Connection>,
	routes: HashMap<IpAddr, EndpointId>,
}

impl Peers {
	/// Replaces the routing table with one derived from a membership roster.
	/// Every member's addresses fall out of its public key, so this needs no
	/// coordination and survives restarts.
	pub async fn set_routes(&self, members: impl IntoIterator<Item = EndpointId>) {
		let mut routes = HashMap::new();
		for id in members {
			let (v4, v6) = virtual_addrs(id.as_bytes());
			routes.insert(IpAddr::V4(v4), id);
			routes.insert(IpAddr::V6(v6), id);
		}
		self.inner.write().await.routes = routes;
	}

	/// Every address we expect to carry, for the system routing table.
	pub async fn routed_addrs(&self) -> HashSet<IpAddr> {
		self.inner.read().await.routes.keys().copied().collect()
	}

	/// Registers a live link, returning false if one already exists for this
	/// peer. Both ends may dial each other at once; the first to arrive wins
	/// and the loser closes rather than leaving two links to the same peer.
	pub async fn register(&self, id: EndpointId, conn: Connection) -> bool {
		match self.inner.write().await.links.entry(id) {
			Entry::Occupied(_) => false,
			Entry::Vacant(slot) => {
				slot.insert(conn);
				true
			}
		}
	}

	pub async fn unregister(&self, id: &EndpointId) {
		self.inner.write().await.links.remove(id);
	}

	/// Closes every link. Each peer's read loop sees the close and unregisters,
	/// but we drain here so nothing is handed out in the meantime.
	pub async fn close_all(&self) {
		for (_, conn) in self.inner.write().await.links.drain() {
			conn.close(CLOSE_LEFT.into(), b"left the network");
		}
	}

	pub async fn route(&self, dst: IpAddr) -> Option<EndpointId> {
		self.inner.read().await.routes.get(&dst).copied()
	}

	pub async fn link(&self, id: &EndpointId) -> Option<Connection> {
		self.inner.read().await.links.get(id).cloned()
	}

	/// Routed peers we have no live link to — the dialer's work queue.
	pub async fn unlinked(&self) -> Vec<EndpointId> {
		let inner = self.inner.read().await;
		let mut ids: Vec<EndpointId> = inner
			.routes
			.values()
			.filter(|id| !inner.links.contains_key(*id))
			.copied()
			.collect();
		// Each peer appears once per address family.
		ids.sort();
		ids.dedup();
		ids
	}

	/// Every member with both its addresses and whether it's currently linked.
	pub async fn snapshot(&self) -> Vec<PeerRow> {
		let inner = self.inner.read().await;

		let mut rows: HashMap<EndpointId, PeerRow> = HashMap::new();
		for (addr, id) in &inner.routes {
			let link = inner.links.get(id);
			let row = rows.entry(*id).or_insert_with(|| PeerRow {
				id: *id,
				v4: Ipv4Addr::UNSPECIFIED,
				v6: Ipv6Addr::UNSPECIFIED,
				linked: link.is_some(),
				datagram_max: link.and_then(|conn| conn.max_datagram_size()),
			});
			match addr {
				IpAddr::V4(v4) => row.v4 = *v4,
				IpAddr::V6(v6) => row.v6 = *v6,
			}
		}

		let mut rows: Vec<PeerRow> = rows.into_values().collect();
		rows.sort_by_key(|row| row.v6);
		rows
	}
}

#[derive(Debug, Clone, Copy)]
pub struct PeerRow {
	pub id: EndpointId,
	pub v4: Ipv4Addr,
	pub v6: Ipv6Addr,
	pub linked: bool,
	pub datagram_max: Option<usize>,
}

#[cfg(test)]
mod tests {
	use super::*;

	fn id(n: u8) -> EndpointId {
		iroh::SecretKey::from_bytes(&[n; 32]).public()
	}

	#[tokio::test]
	async fn both_families_route_to_the_same_peer() {
		let peers = Peers::default();
		peers.set_routes([id(1)]).await;

		let (v4, v6) = virtual_addrs(id(1).as_bytes());
		assert_eq!(peers.route(IpAddr::V4(v4)).await, Some(id(1)));
		assert_eq!(peers.route(IpAddr::V6(v6)).await, Some(id(1)));
	}

	#[tokio::test]
	async fn unknown_addresses_have_no_route() {
		let peers = Peers::default();
		peers.set_routes([id(1)]).await;

		assert_eq!(peers.route("8.8.8.8".parse().unwrap()).await, None);
		assert_eq!(peers.route("2001:4860::8888".parse().unwrap()).await, None);
	}

	#[tokio::test]
	async fn set_routes_replaces_rather_than_merges() {
		let peers = Peers::default();
		peers.set_routes([id(1)]).await;
		peers.set_routes([id(2)]).await;

		let (gone, _) = virtual_addrs(id(1).as_bytes());
		assert_eq!(peers.route(IpAddr::V4(gone)).await, None);
		assert_eq!(peers.routed_addrs().await.len(), 2, "one peer, two families");
	}

	#[tokio::test]
	async fn a_peer_is_listed_once_despite_two_routes() {
		let peers = Peers::default();
		peers.set_routes([id(1), id(2)]).await;

		assert_eq!(peers.unlinked().await.len(), 2, "peers, not addresses");
		assert_eq!(peers.routed_addrs().await.len(), 4);

		let rows = peers.snapshot().await;
		assert_eq!(rows.len(), 2);
		assert!(rows.iter().all(|row| !row.v4.is_unspecified()));
		assert!(rows.iter().all(|row| !row.v6.is_unspecified()));
		assert!(rows.iter().all(|row| !row.linked));
	}
}
