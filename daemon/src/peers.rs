use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use iroh::endpoint::Connection;
use iroh::EndpointId;
use tokio::sync::RwLock;

use crate::tun::virtual_ipv4;

/// The mesh's forwarding state: which virtual IP belongs to which peer, and
/// which of those peers we currently hold a live connection to.
///
/// Routes come from the membership roster and exist whether or not the peer is
/// reachable; links come and go as connections are established and dropped.
/// A packet for a routed-but-unlinked peer is what triggers a dial.
#[derive(Clone, Default)]
pub struct Peers {
	inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
	links: HashMap<EndpointId, Connection>,
	routes: HashMap<Ipv4Addr, EndpointId>,
}

impl Peers {
	/// Replaces the routing table with one derived from a membership roster.
	/// Every member's address falls out of its public key, so this needs no
	/// coordination and survives restarts.
	pub async fn set_routes(&self, members: impl IntoIterator<Item = EndpointId>) {
		let routes = members
			.into_iter()
			.map(|id| (virtual_ipv4(id.as_bytes()), id))
			.collect();
		self.inner.write().await.routes = routes;
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

	pub async fn route(&self, dst: Ipv4Addr) -> Option<EndpointId> {
		self.inner.read().await.routes.get(&dst).copied()
	}

	pub async fn link(&self, id: &EndpointId) -> Option<Connection> {
		self.inner.read().await.links.get(id).cloned()
	}

	/// Routed peers we have no live link to — the dialer's work queue.
	pub async fn unlinked(&self) -> Vec<EndpointId> {
		let inner = self.inner.read().await;
		inner
			.routes
			.values()
			.filter(|id| !inner.links.contains_key(*id))
			.copied()
			.collect()
	}

	/// Every routed peer with its address and whether it's currently linked.
	pub async fn snapshot(&self) -> Vec<(EndpointId, Ipv4Addr, bool)> {
		let inner = self.inner.read().await;
		let mut rows: Vec<_> = inner
			.routes
			.iter()
			.map(|(ip, id)| (*id, *ip, inner.links.contains_key(id)))
			.collect();
		rows.sort_by_key(|(_, ip, _)| *ip);
		rows
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn id(n: u8) -> EndpointId {
		iroh::SecretKey::from_bytes(&[n; 32]).public()
	}

	#[tokio::test]
	async fn routes_map_a_members_address_back_to_it() {
		let peers = Peers::default();
		peers.set_routes([id(1), id(2)]).await;

		assert_eq!(peers.route(virtual_ipv4(id(1).as_bytes())).await, Some(id(1)));
		assert_eq!(peers.route(virtual_ipv4(id(2).as_bytes())).await, Some(id(2)));
		assert_eq!(peers.route(Ipv4Addr::new(8, 8, 8, 8)).await, None);
	}

	#[tokio::test]
	async fn set_routes_replaces_rather_than_merges() {
		let peers = Peers::default();
		peers.set_routes([id(1)]).await;
		peers.set_routes([id(2)]).await;

		assert_eq!(peers.route(virtual_ipv4(id(1).as_bytes())).await, None);
		assert_eq!(peers.route(virtual_ipv4(id(2).as_bytes())).await, Some(id(2)));
	}

	#[tokio::test]
	async fn every_routed_peer_starts_unlinked() {
		let peers = Peers::default();
		peers.set_routes([id(1), id(2)]).await;

		let mut unlinked = peers.unlinked().await;
		unlinked.sort();
		let mut expected = vec![id(1), id(2)];
		expected.sort();

		assert_eq!(unlinked, expected);
		assert!(peers.link(&id(1)).await.is_none());
	}

	#[tokio::test]
	async fn snapshot_lists_routed_peers_in_address_order() {
		let peers = Peers::default();
		peers.set_routes([id(1), id(2), id(3)]).await;

		let rows = peers.snapshot().await;
		assert_eq!(rows.len(), 3);
		assert!(rows.windows(2).all(|w| w[0].1 <= w[1].1), "sorted by address");
		assert!(rows.iter().all(|(_, _, linked)| !linked));
	}
}
