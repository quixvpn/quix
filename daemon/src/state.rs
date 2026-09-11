use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tun_rs::AsyncDevice;

#[derive(Clone)]
pub struct State {
	peer_count: Arc<AtomicU64>,
	pub tun: Arc<AsyncDevice>,
}

impl fmt::Debug for State {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("State")
			.field("peer_count", &self.peer_count())
			.finish()
	}
}

impl State {
	pub fn new(tun: AsyncDevice) -> Self {
		Self {
			peer_count: Arc::new(AtomicU64::new(0)),
			tun: Arc::new(tun),
		}
	}

	pub fn peer_connected(&self) {
		self.peer_count.fetch_add(1, Ordering::Relaxed);
	}

	pub fn peer_disconnected(&self) {
		self.peer_count.fetch_sub(1, Ordering::Relaxed);
	}

	pub fn peer_count(&self) -> u64 {
		self.peer_count.load(Ordering::Relaxed)
	}
}