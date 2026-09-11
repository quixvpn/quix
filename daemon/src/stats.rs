use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Counters for each hop a packet takes, so a silent failure can be located
/// without a packet capture: whether packets reach the TUN at all, whether
/// they find a route and a link, and whether they arrive at the far end.
#[derive(Clone, Default)]
pub struct Stats {
	inner: Arc<Counters>,
}

#[derive(Default)]
struct Counters {
	tun_rx: AtomicU64,
	tun_tx: AtomicU64,
	mesh_tx: AtomicU64,
	mesh_rx: AtomicU64,
	no_route: AtomicU64,
	no_link: AtomicU64,
	oversize: AtomicU64,
	send_err: AtomicU64,
	tun_tx_err: AtomicU64,
}

/// A flat snapshot, in the order a packet would visit them.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
	pub tun_rx: u64,
	pub tun_tx: u64,
	pub mesh_tx: u64,
	pub mesh_rx: u64,
	pub no_route: u64,
	pub no_link: u64,
	pub oversize: u64,
	pub send_err: u64,
	pub tun_tx_err: u64,
}

impl Stats {
	/// Read off the TUN, i.e. an application sent it into the mesh.
	pub fn tun_rx(&self) {
		self.inner.tun_rx.fetch_add(1, Ordering::Relaxed);
	}

	/// Written to the TUN, i.e. delivered to a local application.
	pub fn tun_tx(&self) {
		self.inner.tun_tx.fetch_add(1, Ordering::Relaxed);
	}

	/// Handed to a peer's connection as a datagram.
	pub fn mesh_tx(&self) {
		self.inner.mesh_tx.fetch_add(1, Ordering::Relaxed);
	}

	/// Received from a peer as a datagram.
	pub fn mesh_rx(&self) {
		self.inner.mesh_rx.fetch_add(1, Ordering::Relaxed);
	}

	/// Dropped: the destination address belongs to no member.
	pub fn no_route(&self) {
		self.inner.no_route.fetch_add(1, Ordering::Relaxed);
	}

	/// Dropped: the destination is a member, but no link is up yet.
	pub fn no_link(&self) {
		self.inner.no_link.fetch_add(1, Ordering::Relaxed);
	}

	/// Dropped: larger than the link's datagram limit.
	pub fn oversize(&self) {
		self.inner.oversize.fetch_add(1, Ordering::Relaxed);
	}

	/// The peer's connection refused the datagram.
	pub fn send_err(&self) {
		self.inner.send_err.fetch_add(1, Ordering::Relaxed);
	}

	/// Writing an inbound packet to the TUN failed.
	pub fn tun_tx_err(&self) {
		self.inner.tun_tx_err.fetch_add(1, Ordering::Relaxed);
	}

	pub fn snapshot(&self) -> Snapshot {
		let c = &self.inner;
		Snapshot {
			tun_rx: c.tun_rx.load(Ordering::Relaxed),
			tun_tx: c.tun_tx.load(Ordering::Relaxed),
			mesh_tx: c.mesh_tx.load(Ordering::Relaxed),
			mesh_rx: c.mesh_rx.load(Ordering::Relaxed),
			no_route: c.no_route.load(Ordering::Relaxed),
			no_link: c.no_link.load(Ordering::Relaxed),
			oversize: c.oversize.load(Ordering::Relaxed),
			send_err: c.send_err.load(Ordering::Relaxed),
			tun_tx_err: c.tun_tx_err.load(Ordering::Relaxed),
		}
	}
}
