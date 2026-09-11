use anyhow::Result;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tun_rs::AsyncDevice;

use crate::membership::Membership;

#[derive(Clone)]
pub struct State {
	peer_count: Arc<AtomicU64>,
	pub tun: Arc<AsyncDevice>,
	membership: Arc<Mutex<Membership>>,
}

impl fmt::Debug for State {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("State")
			.field("peer_count", &self.peer_count())
			.finish()
	}
}

impl State {
	pub fn new(tun: AsyncDevice) -> Result<Self> {
		Ok(Self {
			peer_count: Arc::new(AtomicU64::new(0)),
			tun: Arc::new(tun),
			membership: Arc::new(Mutex::new(Membership::load()?)),
		})
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

	pub async fn create_network(&self, name: String, own_id: String) -> Result<()> {
		let mut m = self.membership.lock().await;
		m.create(name, own_id);
		m.save()
	}

	pub async fn generate_invite(&self) -> Result<String> {
		let mut m = self.membership.lock().await;
		let token = m.generate_invite();
		m.save()?;
		Ok(token)
	}

	pub async fn redeem_invite(&self, token: &str, requester_id: String) -> Result<bool> {
		let mut m = self.membership.lock().await;
		let ok = m.redeem_invite(token, requester_id);
		if ok {
			m.save()?;
		}
		Ok(ok)
	}

	pub async fn set_joined(
		&self,
		coordinator_id: String,
		own_id: String,
		name: Option<String>,
	) -> Result<()> {
		let mut m = self.membership.lock().await;
		m.set_joined(coordinator_id, own_id, name);
		m.save()
	}

	pub async fn is_member(&self, id: &str) -> bool {
		self.membership.lock().await.is_member(id)
	}

	pub async fn is_coordinator(&self, own_id: &str) -> bool {
		self.membership.lock().await.is_coordinator(own_id)
	}

	pub async fn network_name(&self) -> Option<String> {
		self.membership.lock().await.network_name.clone()
	}
}