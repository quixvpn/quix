use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use iroh::EndpointId;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use crate::names;

/// An outstanding invite, and the two things that decide whether it still counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingInvite {
	/// The network this was minted under. Tokens do not survive the node moving
	/// to a different network, so a code shared for one network cannot admit
	/// anyone to the next one.
	pub network_id: Option<String>,
	/// When it stops being redeemable.
	pub expires_at: DateTime<Utc>,
}

impl PendingInvite {
	fn is_live(&self, network_id: Option<&str>, now: DateTime<Utc>) -> bool {
		// Both have to hold. An unbound invite matches no network, which is how
		// tokens written before this existed are refused.
		self.network_id.as_deref() == network_id
			&& self.network_id.is_some()
			&& self.expires_at > now
	}
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Membership {
	pub network_name: Option<String>,
	pub coordinator_id: Option<String>,
	/// Distinguishes one network from the next even when the same node
	/// coordinates both under the same name. Local to this node: invites are
	/// checked by whoever minted them, so it never goes on the wire.
	#[serde(default)]
	pub network_id: Option<String>,
	pub members: Vec<Member>,
	#[serde(default, with = "hex_tokens")]
	pending_invites: HashMap<[u8; 16], PendingInvite>,
	/// Which key owns each hostname, including names whose owner has since
	/// left. Entries are never rewritten to a different key, so a roster push
	/// cannot silently redirect a name that is already in use — see
	/// `set_roster`. Reclaiming one takes a deliberate, announced override.
	#[serde(default)]
	bindings: BTreeMap<String, String>,
}

/// One peer in the roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Member {
	pub id: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub hostname: Option<String>,
}

impl Member {
	pub fn new(id: String) -> Self {
		Self { id, hostname: None }
	}
}

impl<'de> Deserialize<'de> for Member {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		// Rosters written before hostnames existed were a bare list of endpoint
		// ids. Accept both shapes so an existing network.json still loads.
		#[derive(Deserialize)]
		#[serde(untagged)]
		enum Repr {
			Legacy(String),
			Full {
				id: String,
				#[serde(default)]
				hostname: Option<String>,
			},
		}

		Ok(match Repr::deserialize(deserializer)? {
			Repr::Legacy(id) => Member::new(id),
			Repr::Full { id, hostname } => Member { id, hostname },
		})
	}
}

/// Invite tokens are 16 raw bytes in memory but hex on disk. Serde would
/// otherwise write them as 16-element number arrays, which are unreadable and
/// wouldn't match the hex the on-the-wire protocol already uses.
mod hex_tokens {
	use super::PendingInvite;
	use serde::{Deserialize, Deserializer, Serialize, Serializer};
	use std::collections::HashMap;

	/// One stored invite. Written as a token beside what it is good for.
	#[derive(Serialize, Deserialize)]
	struct Entry {
		token: String,
		#[serde(flatten)]
		invite: PendingInvite,
	}

	/// What a `pending_invites` array may hold. Files written before invites
	/// expired carry bare hex strings.
	#[derive(Deserialize)]
	#[serde(untagged)]
	enum Repr {
		Full(Entry),
		// The token is never read — these are dropped — but the field has to be
		// here for the variant to match a bare JSON string at all.
		Legacy(#[allow(dead_code)] String),
	}

	pub fn serialize<S: Serializer>(
		invites: &HashMap<[u8; 16], PendingInvite>,
		serializer: S,
	) -> Result<S::Ok, S::Error> {
		let mut entries: Vec<Entry> = invites
			.iter()
			.map(|(token, invite)| Entry {
				token: hex::encode(token),
				invite: invite.clone(),
			})
			.collect();
		// Stable output, so saving twice gives the same file.
		entries.sort_by(|a, b| a.token.cmp(&b.token));
		entries.serialize(serializer)
	}

	pub fn deserialize<'de, D: Deserializer<'de>>(
		deserializer: D,
	) -> Result<HashMap<[u8; 16], PendingInvite>, D::Error> {
		let mut out = HashMap::new();

		for entry in Vec::<Repr>::deserialize(deserializer)? {
			// A token from before invites were bound and dated carries neither,
			// so there is no window and no network it could be honoured for.
			// Dropping it is the whole point: those are the tokens that used to
			// outlive their network.
			let Repr::Full(entry) = entry else { continue };

			let bytes = hex::decode(&entry.token).map_err(serde::de::Error::custom)?;
			let token: [u8; 16] = bytes
				.try_into()
				.map_err(|_| serde::de::Error::custom("invite token must be 16 bytes"))?;
			out.insert(token, entry.invite);
		}

		Ok(out)
	}
}

/// Names one network apart from another. Random rather than derived: two
/// networks created by the same node under the same name must not collide, and
/// that is exactly the case that let a stale invite cross over.
fn new_network_id() -> String {
	let mut bytes = [0u8; 16];
	getrandom::fill(&mut bytes).expect("failed to get random bytes");
	hex::encode(bytes)
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
			Ok(data) => Self::parse(&data).with_context(|| format!("parse {}", path.display())),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
			Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
		}
	}

	/// Parses the roster, tolerating a leading byte order mark.
	///
	/// Nothing we write adds one, but Notepad and PowerShell both do, and this
	/// file failing to parse stops the daemon starting. Three invisible bytes
	/// should not cost someone their mesh.
	fn parse(data: &str) -> Result<Self> {
		Ok(serde_json::from_str(data.trim_start_matches('\u{feff}'))?)
	}

	pub fn save(&self) -> Result<()> {
		let path = path()?;
		if let Some(parent) = path.parent() {
			std::fs::create_dir_all(parent)?;
		}
		std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
		Ok(())
	}

	pub fn create(&mut self, name: String, own_id: String, hostname: Option<String>) {
		self.network_name = Some(name);
		self.coordinator_id = Some(own_id.clone());
		// A new network is a different network, whatever it is called, so
		// invites minted under the last one stop counting here.
		self.network_id = Some(new_network_id());
		self.pending_invites.clear();
		self.members = vec![Member::new(own_id.clone())];
		if let Some(hostname) = hostname {
			// Validated by the caller; we are the coordinator and alone, so
			// there is nothing yet to collide with.
			self.bind(&own_id, &hostname);
		}
	}

	/// Mints a one-time invite, returning the full shareable code (id + token,
	/// Base58-encoded as one blob — shorter than hex, and nothing to split)
	/// alongside the moment it stops being redeemable.
	///
	/// The window and the network are recorded here, not in the code: the
	/// coordinator is the only party that checks them, so putting them on the
	/// wire would only invite a joiner to argue about them.
	pub fn generate_invite(
		&mut self,
		own_id: EndpointId,
		ttl: Duration,
		now: DateTime<Utc>,
	) -> (String, DateTime<Utc>) {
		// Cheapest possible moment to take out the rubbish.
		self.prune_expired(now);

		let mut token = [0u8; 16];
		getrandom::fill(&mut token).expect("failed to get random bytes");

		let expires_at = now + ttl;
		self.pending_invites.insert(
			token,
			PendingInvite {
				network_id: self.network_id.clone(),
				expires_at,
			},
		);

		let mut blob = Vec::with_capacity(48);
		blob.extend_from_slice(own_id.as_bytes());
		blob.extend_from_slice(&token);
		(bs58::encode(blob).into_string(), expires_at)
	}

	/// Forgets invites whose window has closed. They can never be redeemed
	/// again, so keeping them only grows the file.
	fn prune_expired(&mut self, now: DateTime<Utc>) {
		self.pending_invites
			.retain(|_, invite| invite.expires_at > now);
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

	pub fn is_coordinator(&self, own_id: &str) -> bool {
		self.coordinator_id.as_deref() == Some(own_id)
	}

	pub fn member(&self, id: &str) -> Option<&Member> {
		self.members.iter().find(|m| m.id == id)
	}

	pub fn is_member(&self, id: &str) -> bool {
		self.member(id).is_some()
	}

	pub fn hostname_of(&self, id: &str) -> Option<&str> {
		self.member(id)?.hostname.as_deref()
	}

	/// The roster, ordered so saving twice produces the same file.
	pub fn roster(&self) -> Vec<Member> {
		let mut roster = self.members.clone();
		roster.sort_by(|a, b| a.id.cmp(&b.id));
		roster
	}

	/// Records a hostname as belonging to a key, in both the roster and the
	/// binding table.
	fn bind(&mut self, id: &str, hostname: &str) {
		self.bindings.insert(hostname.to_string(), id.to_string());
		if let Some(member) = self.members.iter_mut().find(|m| m.id == id) {
			member.hostname = Some(hostname.to_string());
		}
	}

	/// Whether a name belongs to some other key — a live member's, a departed
	/// member's tombstone, or another member's unforgeable fallback.
	fn spoken_for(&self, name: &str, claimant: &str) -> bool {
		let bound_elsewhere = self
			.bindings
			.get(name)
			.is_some_and(|owner| owner != claimant);

		let shadows_a_fallback = self
			.members
			.iter()
			.any(|m| m.id != claimant && names::fallback(&m.id) == name);

		bound_elsewhere || shadows_a_fallback
	}

	/// Coordinator side of a hostname claim: validates, resolves collisions by
	/// suffix, and records the binding. Returns the name actually assigned,
	/// which may differ from the one requested.
	///
	/// Bindings are append-only — a name never moves to a different key here,
	/// which is what stops a later roster push from redirecting it. `force`
	/// exists for the one case that cannot be served otherwise: a machine
	/// rebuilt under a new key reclaiming the name it used to hold.
	pub fn claim_hostname(
		&mut self,
		claimant: &str,
		requested: &str,
		force: bool,
	) -> Result<String, String> {
		let wanted = names::validate(requested, claimant)?;

		if force {
			self.bindings.remove(&wanted);
			// The previous holder keeps its roster entry but loses the name,
			// so two members never claim it at once.
			if let Some(previous) = self.members.iter_mut().find(|m| m.hostname.as_deref() == Some(wanted.as_str())) {
				previous.hostname = None;
			}
		}

		let assigned = names::dedupe(&wanted, claimant, |candidate| {
			self.spoken_for(candidate, claimant)
		});
		self.bind(claimant, &assigned);
		Ok(assigned)
	}

	/// Burns an invite and admits the bearer, if it is still good for anything.
	///
	/// Being unknown, already spent, past its window, or minted for a different
	/// network are all the same answer — a joiner learns only that the code did
	/// not work, never which of those it was.
	pub fn redeem_invite(
		&mut self,
		token: &[u8; 16],
		requester_id: String,
		now: DateTime<Utc>,
	) -> bool {
		let network_id = self.network_id.clone();

		// Removed before it is judged: a token presented once is spent whatever
		// the verdict, so a rejected code cannot be retried against a network
		// this node moves to later.
		let Some(invite) = self.pending_invites.remove(token) else {
			return false;
		};
		self.prune_expired(now);

		if !invite.is_live(network_id.as_deref(), now) {
			return false;
		}

		if !self.is_member(&requester_id) {
			self.members.push(Member::new(requester_id));
		}
		true
	}

	/// Records the network we were admitted to, along with the roster the
	/// coordinator handed back. Without that roster a joiner would only ever
	/// trust the coordinator, and members #2 and #3 would reject each other.
	pub fn set_joined(
		&mut self,
		coordinator_id: String,
		own_id: String,
		name: Option<String>,
		roster: Vec<Member>,
	) {
		self.coordinator_id = Some(coordinator_id.clone());
		self.network_name = name;
		self.members.clear();
		// A fresh network means no prior bindings to honour; everything in this
		// first roster is what we pin from here on.
		self.bindings.clear();
		// Someone else coordinates here, so we mint nothing and anything we
		// minted before belonged to a network we have now left.
		self.network_id = None;
		self.pending_invites.clear();
		self.apply_roster(&own_id, roster);
	}

	/// Applies a roster pushed by the coordinator, refusing any hostname that
	/// would move to a different key than the one we first saw holding it.
	///
	/// Returns a description of each refusal, for logging and for `status`.
	/// This is where the binding rule actually bites: enforcing it only at the
	/// coordinator would do nothing against a coordinator that is itself the
	/// problem.
	pub fn set_roster(
		&mut self,
		own_id: &str,
		name: Option<String>,
		roster: Vec<Member>,
	) -> Vec<String> {
		if name.is_some() {
			self.network_name = name;
		}
		self.members.clear();
		self.apply_roster(own_id, roster)
	}

	fn apply_roster(&mut self, own_id: &str, roster: Vec<Member>) -> Vec<String> {
		let mut conflicts = Vec::new();

		for mut member in roster {
			if let Some(hostname) = member.hostname.clone() {
				match self.bindings.get(&hostname) {
					Some(owner) if owner != &member.id => {
						conflicts.push(format!(
							"{hostname} belongs to {}; refused to rebind it to {}",
							names::fallback(owner),
							names::fallback(&member.id)
						));
						// Drop the disputed name but keep the member: naming and
						// reachability should not fail together.
						member.hostname = None;
					}
					_ => {
						self.bindings.insert(hostname, member.id.clone());
					}
				}
			}
			if !self.is_member(&member.id) {
				self.members.push(member);
			}
		}

		// We and the coordinator stay in our own roster whatever arrives, so a
		// malformed push cannot evict us from our own network.
		for id in [Some(own_id.to_string()), self.coordinator_id.clone()]
			.into_iter()
			.flatten()
		{
			if !self.is_member(&id) {
				self.members.push(Member::new(id));
			}
		}

		conflicts
	}

	/// Forgets the network entirely. Invites and bindings go too — they only
	/// describe the network we are leaving.
	pub fn leave(&mut self) {
		self.network_name = None;
		self.coordinator_id = None;
		self.network_id = None;
		self.members.clear();
		self.pending_invites.clear();
		self.bindings.clear();
	}

	/// Coordinator side of someone leaving. Returns whether they were listed.
	///
	/// Their hostname binding is deliberately kept: a departure is only ever
	/// reported to other members by the coordinator, so freeing the name here
	/// would let a compromised one evict a peer and take its name.
	pub fn remove_member(&mut self, id: &str) -> bool {
		let before = self.members.len();
		self.members.retain(|m| m.id != id);
		before != self.members.len()
	}

	/// The roster as endpoint ids, skipping any entry that doesn't parse.
	pub fn member_ids(&self) -> Vec<EndpointId> {
		self.members.iter().filter_map(|m| m.id.parse().ok()).collect()
	}
}
#[cfg(test)]
mod tests {
	use super::*;

	fn id(n: u8) -> String {
		iroh::SecretKey::from_bytes(&[n; 32]).public().to_string()
	}

	fn coordinator() -> EndpointId {
		iroh::SecretKey::from_bytes(&[1u8; 32]).public()
	}

	/// The instant these tests pretend an invite was minted at.
	///
	/// Arbitrary and fixed. Expiry is the thing under test, so the clock has to
	/// be an input rather than the wall clock — otherwise "still valid after 29
	/// minutes" depends on how long the test took to run. This is why
	/// `generate_invite` and `redeem_invite` take the time instead of reading it.
	fn minted_at() -> DateTime<Utc> {
		"2000-01-01T00:00:00Z".parse().unwrap()
	}

	/// A network with one outstanding invite, which is the state every one of
	/// these tests starts from.
	fn with_invite(name: &str, ttl: Duration) -> (Membership, [u8; 16]) {
		let mut m = Membership::default();
		m.create(name.into(), id(1), None);
		let (code, _) = m.generate_invite(coordinator(), ttl, minted_at());
		let (_, token) = Membership::decode_invite(&code).unwrap();
		(m, token)
	}

	#[test]
	fn invite_code_round_trips() {
		let mut m = Membership::default();
		m.create("net".into(), id(1), None);

		let (code, expires_at) = m.generate_invite(coordinator(), Duration::minutes(5), minted_at());
		let (decoded_id, token) = Membership::decode_invite(&code).unwrap();

		assert_eq!(decoded_id, coordinator());
		assert_eq!(expires_at, minted_at() + Duration::minutes(5));
		assert!(m.redeem_invite(&token, id(2), minted_at()), "minted token must redeem");
		assert!(m.is_member(&id(2)));
	}

	#[test]
	fn an_invite_only_redeems_once() {
		let (mut m, token) = with_invite("net", Duration::minutes(5));

		assert!(m.redeem_invite(&token, id(2), minted_at()));
		assert!(!m.redeem_invite(&token, id(3), minted_at()), "token must be burned");
		assert!(!m.is_member(&id(3)));
	}

	#[test]
	fn a_single_use_invite_is_spent_even_with_time_left() {
		// The window is a ceiling, not an allowance: redeeming consumes it.
		let (mut m, token) = with_invite("net", Duration::days(2));

		assert!(m.redeem_invite(&token, id(2), minted_at()));
		let later = minted_at() + Duration::hours(1);
		assert!(!m.redeem_invite(&token, id(3), later), "still single use");
	}

	#[test]
	fn an_invite_expires_once_its_window_passes() {
		let (mut m, token) = with_invite("net", Duration::minutes(30));

		let too_late = minted_at() + Duration::minutes(31);
		assert!(!m.redeem_invite(&token, id(2), too_late), "the window closed");
		assert!(!m.is_member(&id(2)), "and nobody was admitted");
	}

	#[test]
	fn an_invite_holds_right_up_to_its_deadline() {
		let (mut m, token) = with_invite("net", Duration::minutes(30));

		// A second before, it still works; the boundary itself is closed, so a
		// stored deadline is never ambiguous.
		assert!(m.redeem_invite(&token, id(2), minted_at() + Duration::minutes(30) - Duration::seconds(1)));

		let (mut m, token) = with_invite("net", Duration::minutes(30));
		assert!(!m.redeem_invite(&token, id(2), minted_at() + Duration::minutes(30)));
	}

	#[test]
	fn a_failed_redemption_still_spends_the_token() {
		// Otherwise a code refused for one reason could be kept and retried
		// against a network this node moves to later.
		let (mut m, token) = with_invite("net", Duration::minutes(30));

		assert!(!m.redeem_invite(&token, id(2), minted_at() + Duration::hours(1)), "expired");
		assert!(!m.redeem_invite(&token, id(2), minted_at()), "and gone even if time rewinds");
	}

	#[test]
	fn an_invite_does_not_survive_creating_another_network() {
		// Vuln 3. A code minted for one network used to admit its bearer to
		// whatever network the node held next.
		let (mut m, token) = with_invite("net-a", Duration::days(2));

		m.create("net-b".into(), id(1), None);

		assert!(!m.redeem_invite(&token, id(9), minted_at()), "net-a's code is not net-b's");
		assert!(!m.is_member(&id(9)));
	}

	#[test]
	fn an_invite_does_not_survive_joining_someone_elses_network() {
		let (mut m, token) = with_invite("net-a", Duration::days(2));

		m.set_joined(id(5), id(1), Some("theirs".into()), vec![member(5)]);

		assert!(!m.redeem_invite(&token, id(9), minted_at()), "we admit nobody here");
		assert!(!m.is_member(&id(9)));
	}

	#[test]
	fn two_networks_of_the_same_name_are_still_different_networks() {
		// The binding cannot lean on the network's name: the same node creating
		// `net` twice is exactly the case a name cannot tell apart.
		let (mut m, token) = with_invite("net", Duration::days(2));
		let first = m.network_id.clone();

		m.create("net".into(), id(1), None);

		assert_ne!(m.network_id, first, "a new network gets a new identity");
		assert!(!m.redeem_invite(&token, id(9), minted_at()));
	}

	#[test]
	fn leaving_invalidates_outstanding_invites() {
		let (mut m, token) = with_invite("net", Duration::days(2));

		m.leave();

		assert!(!m.redeem_invite(&token, id(9), minted_at()));
	}

	#[test]
	fn expired_invites_are_forgotten_rather_than_kept_forever() {
		let mut m = Membership::default();
		m.create("net".into(), id(1), None);
		for _ in 0..3 {
			m.generate_invite(coordinator(), Duration::minutes(5), minted_at());
		}
		assert_eq!(m.pending_invites.len(), 3);

		// Minting after they lapse clears them out.
		m.generate_invite(coordinator(), Duration::minutes(5), minted_at() + Duration::hours(1));
		assert_eq!(m.pending_invites.len(), 1, "only the live one remains");
	}

	#[test]
	fn a_token_from_before_invites_expired_is_refused() {
		// v0.1.x wrote bare hex strings with no window and no network. There is
		// no window to honour and no network to honour it for, and these are
		// precisely the tokens that used to outlive their network.
		//
		// Built from `id()` rather than pasted, so the key is self-evidently a
		// test one and no real node's identity ends up in the repository.
		const LEGACY_TOKEN: &str = "000102030405060708090a0b0c0d0e0f";
		let stored = format!(
			r#"{{
				"network_name": "net",
				"coordinator_id": "{coordinator}",
				"members": ["{coordinator}"],
				"pending_invites": ["{LEGACY_TOKEN}"]
			}}"#,
			coordinator = id(1),
		);

		let mut m: Membership = serde_json::from_str(&stored).expect("an existing file must load");

		assert!(m.pending_invites.is_empty(), "dropped on load");
		let token = hex::decode(LEGACY_TOKEN).unwrap();
		assert!(!m.redeem_invite(&token.try_into().unwrap(), id(9), minted_at()));
	}

	#[test]
	fn an_invite_survives_being_written_and_read_back() {
		let (m, token) = with_invite("net", Duration::days(2));

		let written = serde_json::to_string(&m).unwrap();
		let mut reloaded: Membership = serde_json::from_str(&written).unwrap();

		assert!(
			reloaded.redeem_invite(&token, id(2), minted_at()),
			"a restart must not invalidate a live invite:\n{written}"
		);
	}

	#[test]
	fn decode_invite_rejects_junk() {
		assert!(Membership::decode_invite("not-base58!").is_err());
		assert!(Membership::decode_invite("abc").is_err(), "too short");
	}

	fn member(n: u8) -> Member {
		Member::new(id(n))
	}

	fn named(n: u8, hostname: &str) -> Member {
		Member {
			id: id(n),
			hostname: Some(hostname.to_string()),
		}
	}

	#[test]
	fn joining_trusts_the_whole_roster_not_just_the_coordinator() {
		let mut m = Membership::default();
		m.set_joined(id(1), id(2), Some("gaming".into()), vec![member(1), member(3)]);

		// Without the roster, members 2 and 3 would reject each other.
		assert!(m.is_member(&id(3)), "peers admitted before us must be trusted");
		assert!(m.is_member(&id(1)) && m.is_member(&id(2)));
		assert_eq!(m.network_name.as_deref(), Some("gaming"));
	}

	#[test]
	fn a_roster_push_cannot_evict_us_or_the_coordinator() {
		let mut m = Membership::default();
		m.set_joined(id(1), id(2), Some("gaming".into()), vec![]);

		m.set_roster(&id(2), None, vec![member(3)]);

		assert!(m.is_member(&id(2)), "we stay in our own network");
		assert!(m.is_member(&id(1)), "the coordinator stays");
		assert!(m.is_member(&id(3)), "the pushed member is added");
	}

	#[test]
	fn a_hostname_is_carried_by_the_roster() {
		let mut m = Membership::default();
		m.set_joined(id(1), id(2), None, vec![named(1, "nas")]);

		assert_eq!(m.hostname_of(&id(1)), Some("nas"));
		assert_eq!(m.hostname_of(&id(2)), None, "we asked for no name");
	}

	#[test]
	fn a_claimed_name_cannot_be_rebound_to_another_key() {
		let mut m = Membership::default();
		m.set_joined(id(1), id(2), None, vec![named(1, "nas")]);

		// The coordinator now claims `nas` belongs to a different peer. This is
		// the vector the binding rule exists to close.
		let conflicts = m.set_roster(&id(2), None, vec![named(3, "nas")]);

		assert_eq!(conflicts.len(), 1, "the refusal must be reported");
		assert!(conflicts[0].contains("nas"));
		assert_eq!(m.hostname_of(&id(3)), None, "the name was not handed over");
		assert!(m.is_member(&id(3)), "but the peer is still reachable");
	}

	#[test]
	fn the_same_key_keeps_its_own_name_across_pushes() {
		let mut m = Membership::default();
		m.set_joined(id(1), id(2), None, vec![named(1, "nas")]);

		let conflicts = m.set_roster(&id(2), None, vec![named(1, "nas")]);

		assert!(conflicts.is_empty(), "re-asserting the same binding is fine");
		assert_eq!(m.hostname_of(&id(1)), Some("nas"));
	}

	#[test]
	fn the_coordinator_deduplicates_a_taken_name() {
		let mut m = Membership::default();
		m.create("net".into(), id(1), Some("web".into()));
		m.members.push(Member::new(id(2)));

		assert_eq!(m.claim_hostname(&id(2), "web", false), Ok("web-1".to_string()));
		assert_eq!(m.hostname_of(&id(1)), Some("web"), "the first holder keeps it");
	}

	#[test]
	fn a_name_stays_reserved_after_its_owner_leaves() {
		let mut m = Membership::default();
		m.create("net".into(), id(1), None);
		m.members.push(Member::new(id(2)));
		m.claim_hostname(&id(2), "nas", false).unwrap();

		assert!(m.remove_member(&id(2)));
		m.members.push(Member::new(id(3)));

		// Departures are only ever reported by the coordinator, so freeing the
		// name here would let a compromised one evict a peer and take its name.
		assert_eq!(m.claim_hostname(&id(3), "nas", false), Ok("nas-1".to_string()));
	}

	#[test]
	fn a_peer_can_return_to_a_name_it_previously_held() {
		let mut m = Membership::default();
		m.create("net".into(), id(1), Some("web".into()));

		m.claim_hostname(&id(1), "nas", false).unwrap();
		// Its own tombstone is not an obstacle to itself.
		assert_eq!(m.claim_hostname(&id(1), "web", false), Ok("web".to_string()));
	}

	#[test]
	fn a_forced_claim_takes_the_name_from_the_previous_holder() {
		let mut m = Membership::default();
		m.create("net".into(), id(1), Some("nas".into()));
		m.members.push(Member::new(id(2)));

		// The rebuilt-machine case: a new key reclaiming a name whose original
		// holder is gone.
		assert_eq!(m.claim_hostname(&id(2), "nas", true), Ok("nas".to_string()));
		assert_eq!(m.hostname_of(&id(2)), Some("nas"));
		assert_eq!(m.hostname_of(&id(1)), None, "two peers never share a name");
	}

	#[test]
	fn a_name_shaped_like_another_peers_fallback_is_refused() {
		let mut m = Membership::default();
		m.create("net".into(), id(1), None);
		m.members.push(Member::new(id(2)));

		let shadow = names::fallback(&id(1));
		assert!(m.claim_hostname(&id(2), &shadow, false).is_err());
	}

	#[test]
	fn leaving_forgets_the_bindings_of_the_network_we_left() {
		let mut m = Membership::default();
		m.create("net".into(), id(1), Some("nas".into()));
		m.leave();

		assert_eq!(m.hostname_of(&id(1)), None);
		assert!(m.members.is_empty());
	}

	#[test]
	fn a_roster_written_before_hostnames_still_loads() {
		// Exactly the shape v0.1.x wrote: members as bare id strings, and no
		// bindings key at all. Keys come from `id()` so nothing in this file
		// names a real node.
		let stored = format!(
			r#"{{
				"network_name": "network",
				"coordinator_id": "{first}",
				"members": ["{first}", "{second}"],
				"pending_invites": ["000102030405060708090a0b0c0d0e0f"]
			}}"#,
			first = id(1),
			second = id(2),
		);

		let m: Membership = serde_json::from_str(&stored).expect("an existing file must load");

		assert_eq!(m.members.len(), 2);
		assert!(m.members.iter().all(|member| member.hostname.is_none()));
		assert!(m.is_member(&id(2)));
	}

	#[test]
	fn both_roster_shapes_parse_and_save_in_the_new_one() {
		let mixed = r#"{
			"members": ["aa", {"id": "bb", "hostname": "nas"}]
		}"#;

		let m: Membership = serde_json::from_str(mixed).unwrap();
		assert_eq!(m.hostname_of("bb"), Some("nas"));
		assert_eq!(m.hostname_of("aa"), None);

		// Saving normalises to the object form.
		let written = serde_json::to_string(&m).unwrap();
		assert!(written.contains(r#"{"id":"aa"}"#), "got {written}");
		assert!(written.contains(r#"{"id":"bb","hostname":"nas"}"#), "got {written}");
	}

	#[test]
	fn member_ids_skips_unparseable_entries() {
		let mut m = Membership::default();
		m.members.push(member(1));
		m.members.push(Member::new("garbage".to_string()));

		assert_eq!(m.member_ids().len(), 1);
	}
}
