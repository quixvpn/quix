use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Membership {
	pub network_name: Option<String>,
	pub coordinator_id: Option<String>,
	pub members: HashSet<String>,
	#[serde(default)]
	pending_invites: HashSet<String>,
}

fn path() -> Result<PathBuf> {
	if let Ok(custom) = std::env::var("QUIX_NETWORK_PATH") {
		return Ok(PathBuf::from(custom));
	}
	let config_dir = dirs::config_dir().context("resolve config dir")?;
	Ok(config_dir.join("quix").join("network.json"))
}

impl Membership {
	pub fn load() -> Result<Self> {
		let path = path()?;
		match std::fs::read_to_string(&path) {
			Ok(data) => serde_json::from_str(&data).context("parse network.json"),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
			Err(e) => Err(e.into()),
		}
	}

	pub fn save(&self) -> Result<()> {
		let path = path()?;
		if let Some(parent) = path.parent() {
			std::fs::create_dir_all(parent)?;
		}
		std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
		Ok(())
	}

	/// Becomes the coordinator of a brand new network.
	pub fn create(&mut self, name: String, own_id: String) {
		self.network_name = Some(name);
		self.coordinator_id = Some(own_id.clone());
		self.members.insert(own_id);
	}

	pub fn is_coordinator(&self, own_id: &str) -> bool {
		self.coordinator_id.as_deref() == Some(own_id)
	}

	/// Coordinator-only: mints a one-time invite token.
	pub fn generate_invite(&mut self) -> String {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("failed to get random bytes");
        let token = hex::encode(bytes);
        self.pending_invites.insert(token.clone());
        token
    }

	/// Coordinator-only: consumes a token, admitting the requester.
	pub fn redeem_invite(&mut self, token: &str, requester_id: String) -> bool {
		if self.pending_invites.remove(token) {
			self.members.insert(requester_id);
			true
		} else {
			false
		}
	}

	/// Joiner-only: records who to trust after a successful join.
	///
	/// NOTE (phase 1 limitation): this only trusts the coordinator and
	/// ourselves. Other members admitted later are NOT automatically
	/// trusted here — that requires syncing the member list, which is
	/// phase 2.
	pub fn set_joined(&mut self, coordinator_id: String, own_id: String, name: Option<String>) {
		self.coordinator_id = Some(coordinator_id.clone());
		self.network_name = name;
		self.members.clear();
		self.members.insert(coordinator_id);
		self.members.insert(own_id);
	}

	pub fn is_member(&self, id: &str) -> bool {
		self.members.contains(id)
	}
}