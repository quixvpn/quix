use anyhow::Result;
use iroh::{Endpoint, EndpointId};
use std::fmt;
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tun_rs::AsyncDevice;

use crate::membership::Membership;
use crate::peers::Peers;

#[derive(Clone)]
pub struct State {
	endpoint: Endpoint,
	pub tun: Arc<AsyncDevice>,
	membership: Arc<Mutex<Membership>>,
	peers: Peers,
	dial_tx: mpsc::Sender<EndpointId>,
}

impl fmt::Debug for State {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("State").field("own_id", &self.own_id()).finish()
	}
}

impl State {
	pub fn new(
		endpoint: Endpoint,
		tun: AsyncDevice,
		dial_tx: mpsc::Sender<EndpointId>,
	) -> Result<Self> {
		Ok(Self {
			endpoint,
			tun: Arc::new(tun),
			membership: Arc::new(Mutex::new(Membership::load()?)),
			peers: Peers::default(),
			dial_tx,
		})
	}

	pub fn own_id(&self) -> EndpointId {
		self.endpoint.id()
	}

	pub fn endpoint(&self) -> &Endpoint {
		&self.endpoint
	}

	pub fn virtual_ip(&self) -> Ipv4Addr {
		crate::tun::virtual_ipv4(self.own_id().as_bytes())
	}

	pub fn peers(&self) -> &Peers {
		&self.peers
	}

	/// Rebuilds the routing table from the roster and asks the dialer to reach
	/// anyone we're not linked to yet. Called after every membership change so
	/// a new member starts carrying traffic without waiting for a tick.
	pub async fn refresh_routes(&self) {
		let members = self.membership.lock().await.member_ids();
		self.peers
			.set_routes(members.into_iter().filter(|id| *id != self.own_id()))
			.await;

		for id in self.peers.unlinked().await {
			self.request_dial(id);
		}
	}

	/// Nudges the dialer. Dropping the request when the queue is full is fine:
	/// the dialer's periodic sweep picks the peer up regardless.
	pub fn request_dial(&self, id: EndpointId) {
		let _ = self.dial_tx.try_send(id);
	}

	pub async fn create_network(&self, name: String) -> Result<()> {
		let mut m = self.membership.lock().await;
		m.create(name, self.own_id().to_string());
		m.save()?;
		drop(m);
		self.refresh_routes().await;
		Ok(())
	}

	pub async fn generate_invite(&self) -> Result<String> {
		let mut m = self.membership.lock().await;
		let code = m.generate_invite(self.own_id());
		m.save()?;
		Ok(code)
	}

	/// Coordinator side of a join: admits the requester and returns the roster
	/// they should start with, or None if the token was bad.
	pub async fn redeem_invite(&self, token: &[u8; 16], requester_id: String) -> Result<Option<Vec<String>>> {
		let mut m = self.membership.lock().await;
		if !m.redeem_invite(token, requester_id) {
			return Ok(None);
		}
		m.save()?;
		let roster = m.roster();
		drop(m);
		self.refresh_routes().await;
		Ok(Some(roster))
	}

	pub async fn set_joined(
		&self,
		coordinator_id: String,
		name: Option<String>,
		roster: Vec<String>,
	) -> Result<()> {
		let mut m = self.membership.lock().await;
		m.set_joined(coordinator_id, self.own_id().to_string(), name, roster);
		m.save()?;
		drop(m);
		self.refresh_routes().await;
		Ok(())
	}

	pub async fn set_roster(&self, name: Option<String>, roster: Vec<String>) -> Result<()> {
		let mut m = self.membership.lock().await;
		m.set_roster(self.own_id().to_string(), name, roster);
		m.save()?;
		drop(m);
		self.refresh_routes().await;
		Ok(())
	}

	pub async fn is_member(&self, id: &str) -> bool {
		self.membership.lock().await.is_member(id)
	}

	pub async fn is_coordinator(&self) -> bool {
		self.membership
			.lock()
			.await
			.is_coordinator(&self.own_id().to_string())
	}

	pub async fn coordinator_id(&self) -> Option<String> {
		self.membership.lock().await.coordinator_id.clone()
	}

	pub async fn network_name(&self) -> Option<String> {
		self.membership.lock().await.network_name.clone()
	}

	pub async fn roster(&self) -> Vec<String> {
		self.membership.lock().await.roster()
	}
}
