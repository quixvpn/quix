use anyhow::{Context, Result};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Membership {
	pub network_name: Option<String>,
	pub coordinator_id: Option<String>,
	pub members: HashSet<String>,
	#[serde(default)]
	pending_invites: HashSet<[u8; 16]>,
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

	pub fn create(&mut self, name: String, own_id: String) {
		self.network_name = Some(name);
		self.coordinator_id = Some(own_id.clone());
		self.members.insert(own_id);
	}

	pub fn is_coordinator(&self, own_id: &str) -> bool {
		self.coordinator_id.as_deref() == Some(own_id)
	}

	/// Mints a one-time invite, returning the full shareable code (id + token,
	/// Base58-encoded as one blob — shorter than hex, and nothing to split).
	pub fn generate_invite(&mut self, own_id: EndpointId) -> String {
		let mut token = [0u8; 16];
		getrandom::fill(&mut token).expect("failed to get random bytes");
		self.pending_invites.insert(token);

		let mut blob = Vec::with_capacity(48);
		blob.extend_from_slice(own_id.as_bytes());
		blob.extend_from_slice(&token);
		bs58::encode(blob).into_string()
	}

	/// Decodes an invite code into (coordinator_id, token).
	pub fn decode_invite(code: &str) -> Result<(EndpointId, [u8; 16])> {
		let bytes = bs58::decode(code).into_vec().context("invalid invite code")?;
		if bytes.len() != 48 {
			anyhow::bail!("invalid invite code length");
		}
		let id_bytes: [u8; 32] = bytes[..32].try_into().unwrap();
		let token: [u8; 16] = bytes[32..].try_into().unwrap();
		let id = EndpointId::from_bytes(&id_bytes).context("invalid endpoint id in code")?;
		Ok((id, token))
	}

	pub fn redeem_invite(&mut self, token: &[u8; 16], requester_id: String) -> bool {
		if self.pending_invites.remove(token) {
			self.members.insert(requester_id);
			true
		} else {
			false
		}
	}

	/// Records the network we were admitted to, along with the roster the
	/// coordinator handed back. Without that roster a joiner would only ever
	/// trust the coordinator, and members #2 and #3 would reject each other.
	pub fn set_joined(
		&mut self,
		coordinator_id: String,
		own_id: String,
		name: Option<String>,
		roster: Vec<String>,
	) {
		self.coordinator_id = Some(coordinator_id.clone());
		self.network_name = name;
		self.members.clear();
		self.members.insert(coordinator_id);
		self.members.insert(own_id);
		self.members.extend(roster);
	}

	/// Applies a roster pushed by the coordinator. We keep ourselves in it
	/// unconditionally so a malformed push can't evict us from our own network.
	pub fn set_roster(&mut self, own_id: String, name: Option<String>, roster: Vec<String>) {
		if name.is_some() {
			self.network_name = name;
		}
		self.members = roster.into_iter().collect();
		self.members.insert(own_id);
		if let Some(coordinator) = self.coordinator_id.clone() {
			self.members.insert(coordinator);
		}
	}

	pub fn is_member(&self, id: &str) -> bool {
		self.members.contains(id)
	}

	pub fn roster(&self) -> Vec<String> {
		let mut roster: Vec<String> = self.members.iter().cloned().collect();
		roster.sort();
		roster
	}

	/// The roster as endpoint ids, skipping any entry that doesn't parse.
	pub fn member_ids(&self) -> Vec<EndpointId> {
		self.members.iter().filter_map(|id| id.parse().ok()).collect()
	}
}
#[cfg(test)]
mod tests {
	use super::*;

	fn id(n: u8) -> String {
		iroh::SecretKey::from_bytes(&[n; 32]).public().to_string()
	}

	#[test]
	fn invite_code_round_trips() {
		let coordinator = iroh::SecretKey::from_bytes(&[1u8; 32]).public();
		let mut m = Membership::default();

		let code = m.generate_invite(coordinator);
		let (decoded_id, token) = Membership::decode_invite(&code).unwrap();

		assert_eq!(decoded_id, coordinator);
		assert!(m.redeem_invite(&token, id(2)), "minted token must redeem");
		assert!(m.is_member(&id(2)));
	}

	#[test]
	fn an_invite_only_redeems_once() {
		let coordinator = iroh::SecretKey::from_bytes(&[1u8; 32]).public();
		let mut m = Membership::default();

		let code = m.generate_invite(coordinator);
		let (_, token) = Membership::decode_invite(&code).unwrap();

		assert!(m.redeem_invite(&token, id(2)));
		assert!(!m.redeem_invite(&token, id(3)), "token must be burned");
		assert!(!m.is_member(&id(3)));
	}

	#[test]
	fn decode_invite_rejects_junk() {
		assert!(Membership::decode_invite("not-base58!").is_err());
		assert!(Membership::decode_invite("abc").is_err(), "too short");
	}

	#[test]
	fn joining_trusts_the_whole_roster_not_just_the_coordinator() {
		let mut m = Membership::default();
		m.set_joined(id(1), id(2), Some("gaming".into()), vec![id(1), id(3)]);

		// Without the roster, members 2 and 3 would reject each other.
		assert!(m.is_member(&id(3)), "peers admitted before us must be trusted");
		assert!(m.is_member(&id(1)) && m.is_member(&id(2)));
		assert_eq!(m.network_name.as_deref(), Some("gaming"));
	}

	#[test]
	fn a_roster_push_cannot_evict_us_or_the_coordinator() {
		let mut m = Membership::default();
		m.set_joined(id(1), id(2), Some("gaming".into()), vec![]);

		m.set_roster(id(2), None, vec![id(3)]);

		assert!(m.is_member(&id(2)), "we stay in our own network");
		assert!(m.is_member(&id(1)), "the coordinator stays");
		assert!(m.is_member(&id(3)), "the pushed member is added");
	}

	#[test]
	fn member_ids_skips_unparseable_entries() {
		let mut m = Membership::default();
		m.members.insert(id(1));
		m.members.insert("garbage".to_string());

		assert_eq!(m.member_ids().len(), 1);
	}
}
