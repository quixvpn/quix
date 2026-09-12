use anyhow::Result;
use iroh::{Endpoint, EndpointId};
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tun_rs::AsyncDevice;

use crate::membership::{Member, Membership};
use crate::peers::Peers;
use crate::routes::Routes;
use crate::settings::Settings;
use crate::stats::Stats;

#[derive(Clone)]
pub struct State {
	endpoint: Endpoint,
	pub tun: Arc<AsyncDevice>,
	membership: Arc<Mutex<Membership>>,
	settings: Arc<Mutex<Settings>>,
	peers: Peers,
	routes: Routes,
	stats: Stats,
	/// Naming conflicts reported by the last roster push.
	conflicts: Arc<Mutex<Vec<String>>>,
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
			// Not fatal, unlike the roster. Settings only name the operator, and
			// failing to read them costs a convenience: without one, mutating
			// commands fall back to needing root or elevation, which is the
			// safe direction. Taking the whole mesh down over it is not.
			settings: Arc::new(Mutex::new(Settings::load().unwrap_or_else(|e| {
				crate::warn!(
					"warning: could not load settings ({e:#})\n\
					 continuing with no operator, so mutating commands will need \
					 root or elevation until this is fixed"
				);
				Settings::default()
			}))),
			peers: Peers::default(),
			routes: Routes::new(crate::tun::interface_name()),
			stats: Stats::default(),
			conflicts: Arc::new(Mutex::new(Vec::new())),
			dial_tx,
		})
	}

	pub fn own_id(&self) -> EndpointId {
		self.endpoint.id()
	}

	pub fn endpoint(&self) -> &Endpoint {
		&self.endpoint
	}

	pub fn virtual_addrs(&self) -> (Ipv4Addr, Ipv6Addr) {
		crate::tun::virtual_addrs(self.own_id().as_bytes())
	}

	pub fn peers(&self) -> &Peers {
		&self.peers
	}

	pub fn stats(&self) -> &Stats {
		&self.stats
	}

	/// Rebuilds the routing table from the roster and asks the dialer to reach
	/// anyone we're not linked to yet. Called after every membership change so
	/// a new member starts carrying traffic without waiting for a tick.
	pub async fn refresh_routes(&self) {
		let members = self.membership.lock().await.member_ids();
		self.peers
			.set_routes(members.into_iter().filter(|id| *id != self.own_id()))
			.await;

		// The kernel needs a host route per member address, or packets never
		// reach the TUN in the first place.
		self.routes.sync(&self.peers.routed_addrs().await).await;

		for id in self.peers.unlinked().await {
			self.request_dial(id);
		}
	}

	/// Nudges the dialer. Dropping the request when the queue is full is fine:
	/// the dialer's periodic sweep picks the peer up regardless.
	pub fn request_dial(&self, id: EndpointId) {
		let _ = self.dial_tx.try_send(id);
	}

	pub async fn create_network(&self, name: String, hostname: Option<String>) -> Result<()> {
		let mut m = self.membership.lock().await;
		m.create(name, self.own_id().to_string(), hostname);
		m.save()?;
		drop(m);
		self.refresh_routes().await;
		Ok(())
	}

	/// Mints an invite valid for `ttl`, returning the code and when it dies.
	pub async fn generate_invite(&self, ttl: chrono::Duration) -> Result<(String, String)> {
		let mut m = self.membership.lock().await;
		let (code, expires_at) = m.generate_invite(self.own_id(), ttl, chrono::Utc::now());
		m.save()?;
		Ok((code, expires_at.to_rfc3339()))
	}

	/// Coordinator side of a join: admits the requester and returns the roster
	/// they should start with, or None if the token was bad.
	/// Coordinator side of a join: admits the requester, assigns the hostname
	/// they asked for (or a deduplicated variant), and returns the roster they
	/// should start with plus the name they actually got.
	pub async fn redeem_invite(
		&self,
		token: &[u8; 16],
		requester_id: String,
		hostname: Option<String>,
	) -> Result<Option<(Vec<Member>, Option<String>)>> {
		let mut m = self.membership.lock().await;
		if !m.redeem_invite(token, requester_id.clone(), chrono::Utc::now()) {
			return Ok(None);
		}

		let assigned = match hostname {
			// A rejected hostname must not fail the join — they are in the
			// network either way, reachable by their fallback name.
			Some(requested) => match m.claim_hostname(&requester_id, &requested, false) {
				Ok(name) => Some(name),
				Err(e) => {
					crate::warn!("hostname {requested:?} from {requester_id} refused: {e}");
					None
				}
			},
			None => None,
		};

		m.save()?;
		let roster = m.roster();
		drop(m);
		self.refresh_routes().await;
		Ok(Some((roster, assigned)))
	}

	/// Coordinator side of a hostname claim, used both for our own name and for
	/// a member asking over the admin protocol.
	pub async fn claim_hostname(
		&self,
		claimant: &str,
		requested: &str,
		force: bool,
	) -> Result<std::result::Result<String, String>> {
		let mut m = self.membership.lock().await;
		let outcome = m.claim_hostname(claimant, requested, force);
		if outcome.is_ok() {
			m.save()?;
		}
		Ok(outcome)
	}

	/// Member side: adopt the name the coordinator assigned us.
	pub async fn adopt_hostname(&self, hostname: String) -> Result<()> {
		let own_id = self.own_id().to_string();
		let mut m = self.membership.lock().await;
		m.claim_hostname(&own_id, &hostname, true).map_err(|e| anyhow::anyhow!(e))?;
		m.save()
	}

	pub async fn hostname(&self) -> Option<String> {
		let own_id = self.own_id().to_string();
		self.membership
			.lock()
			.await
			.hostname_of(&own_id)
			.map(str::to_string)
	}

	pub async fn set_joined(
		&self,
		coordinator_id: String,
		name: Option<String>,
		roster: Vec<Member>,
	) -> Result<()> {
		let mut m = self.membership.lock().await;
		m.set_joined(coordinator_id, self.own_id().to_string(), name, roster);
		m.save()?;
		drop(m);
		self.refresh_routes().await;
		Ok(())
	}

	pub async fn set_roster(&self, name: Option<String>, roster: Vec<Member>) -> Result<()> {
		let own_id = self.own_id().to_string();
		let mut m = self.membership.lock().await;
		let conflicts = m.set_roster(&own_id, name, roster);
		m.save()?;
		drop(m);

		// A refused rebinding is the security property doing its job, and the
		// only signal the operator gets, so it must not be silent.
		for conflict in &conflicts {
			crate::warn!("roster conflict: {conflict}");
		}
		*self.conflicts.lock().await = conflicts;

		self.refresh_routes().await;
		Ok(())
	}

	/// Naming conflicts from the most recent roster push, surfaced in `status`.
	pub async fn conflicts(&self) -> Vec<String> {
		self.conflicts.lock().await.clone()
	}

	/// Leaves the network: forgets the roster, tears down the routes it put in
	/// the system table, and drops every link. Returns the network's name.
	pub async fn leave(&self) -> Result<Option<String>> {
		let mut m = self.membership.lock().await;
		let name = m.network_name.clone();
		m.leave();
		m.save()?;
		drop(m);

		self.refresh_routes().await;
		self.peers.close_all().await;
		Ok(name)
	}

	/// Coordinator side of a member leaving.
	pub async fn remove_member(&self, id: &str) -> Result<bool> {
		let mut m = self.membership.lock().await;
		if !m.remove_member(id) {
			return Ok(false);
		}
		m.save()?;
		drop(m);
		self.refresh_routes().await;
		Ok(true)
	}

	/// Who may mutate without being root or elevated, on either platform.
	pub async fn operator(&self) -> crate::authz::Operator {
		let settings = self.settings.lock().await;
		crate::authz::Operator {
			uid: settings.operator_uid,
			sid: settings.operator_sid.clone(),
		}
	}

	pub async fn set_operator(&self, name: String, uid: u32) -> Result<()> {
		let mut settings = self.settings.lock().await;
		settings.operator_uid = Some(uid);
		settings.operator_name = Some(name);
		settings.save()
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

	pub async fn roster(&self) -> Vec<Member> {
		self.membership.lock().await.roster()
	}

	/// Every member's id and overlay addresses, including our own — the
	/// resolver has to answer for this node as well as its peers.
	pub async fn all_addrs(&self) -> Vec<(String, Ipv4Addr, Ipv6Addr)> {
		self.membership
			.lock()
			.await
			.roster()
			.into_iter()
			.map(|m| {
				let (v4, v6) = crate::tun::virtual_addrs(
					&m.id.parse::<EndpointId>().map(|id| id.as_bytes().to_vec()).unwrap_or_default(),
				);
				(m.id, v4, v6)
			})
			.collect()
	}

	/// Hostnames by endpoint id, for status rendering and DNS answers.
	pub async fn hostnames(&self) -> std::collections::HashMap<String, String> {
		self.membership
			.lock()
			.await
			.roster()
			.into_iter()
			.filter_map(|m| m.hostname.map(|h| (m.id, h)))
			.collect()
	}
}
