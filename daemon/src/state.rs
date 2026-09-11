use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Clone, Default, Debug)]
pub struct State {
	peer_count: Arc<AtomicU64>,
}

impl State {
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