//! Hostnames, the fallback identifier, and how a human refers to a peer.
//!
//! Kept separate from `Membership` so anything that targets a peer by name —
//! `status` today, `kick` later — resolves it the same way.

/// DNS label limit, which is what a hostname becomes under `.quix`.
pub const MAX_LEN: usize = 63;

/// How many hex characters of the endpoint id make up a peer's fallback name.
const FALLBACK_LEN: usize = 8;

/// The name a peer always has, derived from its public key.
///
/// Unforgeable by construction: no roster, compromised or otherwise, can point
/// this at a different key, so every peer stays addressable by *some* correct
/// name even while a hostname is disputed. Also a valid DNS label already,
/// being lowercase hex.
pub fn fallback(endpoint_id: &str) -> String {
	endpoint_id.chars().take(FALLBACK_LEN).collect()
}

/// What to show a human: the hostname when there is one, the fallback otherwise.
pub fn display(endpoint_id: &str, hostname: Option<&str>) -> String {
	hostname
		.map(str::to_string)
		.unwrap_or_else(|| fallback(endpoint_id))
}

/// Whether a name is shaped like someone's fallback identifier.
///
/// Claiming one as a vanity hostname would shadow another peer's unforgeable
/// name, so this is refused unless it is the claimant's own.
pub fn looks_like_fallback(name: &str) -> bool {
	name.len() == FALLBACK_LEN && name.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
}

/// Checks a requested hostname against DNS label rules, rejecting at the point
/// of setting rather than leaving something unresolvable in the roster.
///
/// Case is normalised rather than refused: `NAS` and `nas` are the same label
/// to DNS, so accepting the former and storing the latter is least surprising.
pub fn validate(requested: &str, claimant_id: &str) -> Result<String, String> {
	let name = requested.trim().to_ascii_lowercase();

	if name.is_empty() {
		return Err("hostname cannot be empty".to_string());
	}
	if name.len() > MAX_LEN {
		return Err(format!(
			"hostname is {} characters; the DNS label limit is {MAX_LEN}",
			name.len()
		));
	}
	if let Some(bad) = name
		.chars()
		.find(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit() && *c != '-')
	{
		return Err(format!(
			"hostname may only contain letters, digits and hyphens; found {bad:?}"
		));
	}
	if name.starts_with('-') || name.ends_with('-') {
		return Err("hostname cannot start or end with a hyphen".to_string());
	}
	if looks_like_fallback(&name) && name != fallback(claimant_id) {
		return Err(format!(
			"{name} is another peer's fallback identifier, which cannot be reassigned"
		));
	}

	Ok(name)
}

/// Turns a network name into a DNS label, so it can sit in `.quix`.
///
/// Sanitises rather than refuses: networks created before names had to be
/// labels still need to resolve, and rejecting them retroactively would leave
/// those meshes with no zone at all.
pub fn network_label(network: &str) -> String {
	let label: String = network
		.trim()
		.to_ascii_lowercase()
		.chars()
		.map(|c| match c.is_ascii_alphanumeric() {
			true => c,
			false => '-',
		})
		.collect();

	let label = label.trim_matches('-');
	match label.is_empty() {
		true => "network".to_string(),
		false => label.chars().take(MAX_LEN).collect(),
	}
}

/// Checks a network name at creation, where we can still say no.
pub fn validate_network(name: &str) -> Result<String, String> {
	let trimmed = name.trim();
	if trimmed.is_empty() {
		return Err("network name cannot be empty".to_string());
	}
	if trimmed.len() > MAX_LEN {
		return Err(format!(
			"network name is {} characters; it becomes a DNS label, so the limit is {MAX_LEN}",
			trimmed.len()
		));
	}
	// The name is shown as typed but resolves by its label, so refuse anything
	// where the two would differ confusingly.
	let label = network_label(trimmed);
	if label != trimmed.to_ascii_lowercase() {
		return Err(format!(
			"network name may only contain letters, digits and hyphens (it becomes {label}.quix)"
		));
	}
	Ok(trimmed.to_ascii_lowercase())
}

/// How far the numeric sequence runs before falling back to something unique
/// by construction. Far past any plausible number of same-named peers.
const MAX_SUFFIX: u32 = 9999;

/// Picks the first free name in the `web`, `web-1`, `web-2` … sequence.
///
/// `taken` decides what is already spoken for; callers pass a closure so this
/// works against live hostnames and released-but-tombstoned ones alike.
pub fn dedupe(requested: &str, claimant_id: &str, taken: impl Fn(&str) -> bool) -> String {
	if !taken(requested) {
		return requested.to_string();
	}

	for suffix in 1..=MAX_SUFFIX {
		let candidate = with_suffix(requested, &suffix.to_string());
		if !taken(&candidate) {
			return candidate;
		}
	}

	// The claimant's fallback belongs to its key alone, so this terminates with
	// a unique name however pathological the roster is.
	with_suffix(requested, &fallback(claimant_id))
}

/// Appends `-suffix`, trimming the base so the result stays a legal DNS label.
fn with_suffix(base: &str, suffix: &str) -> String {
	let tag = format!("-{suffix}");
	let keep = MAX_LEN.saturating_sub(tag.len());
	// Trimming can expose a trailing hyphen, which would not be a legal label.
	let trimmed = base.chars().take(keep).collect::<String>();
	let trimmed = trimmed.trim_end_matches('-');

	match trimmed.is_empty() {
		true => suffix.to_string(),
		false => format!("{trimmed}{tag}"),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// An invented endpoint id — 64 hex characters, like the real thing, but
	/// nobody's. Nothing in this repository should name a machine that exists.
	const ID: &str = "1234abcd00000000000000000000000000000000000000000000000000000000";

	#[test]
	fn fallback_is_the_first_eight_hex_characters() {
		assert_eq!(fallback(ID), "1234abcd");
		// Stable: the same id always gives the same name.
		assert_eq!(fallback(ID), fallback(ID));
	}

	#[test]
	fn display_prefers_the_hostname_and_falls_back_otherwise() {
		assert_eq!(display(ID, Some("nas")), "nas");
		assert_eq!(display(ID, None), "1234abcd");
	}

	#[test]
	fn valid_hostnames_are_accepted_and_lowercased() {
		assert_eq!(validate("nas", ID), Ok("nas".to_string()));
		assert_eq!(validate("web-1", ID), Ok("web-1".to_string()));
		assert_eq!(validate("  NAS  ", ID), Ok("nas".to_string()));
		assert_eq!(validate("a", ID), Ok("a".to_string()));
	}

	#[test]
	fn invalid_hostnames_are_rejected_with_a_reason() {
		for bad in ["", "   ", "-nas", "nas-", "na s", "na_s", "nas.local", "nås"] {
			assert!(validate(bad, ID).is_err(), "{bad:?} should be rejected");
		}
		assert!(validate(&"a".repeat(MAX_LEN + 1), ID).is_err(), "too long");
		assert!(validate(&"a".repeat(MAX_LEN), ID).is_ok(), "exactly at the limit");
	}

	#[test]
	fn a_fallback_shaped_name_is_only_claimable_by_its_owner() {
		// Someone else's fallback would shadow their unforgeable name.
		assert!(validate("aabbccdd", ID).is_err());
		// Your own is harmless — it is what you already answer to.
		assert!(validate("1234abcd", ID).is_ok());
		// Eight characters that are not hex are an ordinary hostname.
		assert!(validate("frontend", ID).is_ok());
	}

	#[test]
	fn a_network_name_becomes_a_label() {
		assert_eq!(network_label("homelab"), "homelab");
		assert_eq!(network_label("minha-rede"), "minha-rede");
		// Older networks were never checked, so they are sanitised, not refused.
		assert_eq!(network_label("My Network"), "my-network");
		assert_eq!(network_label("  rede!  "), "rede");
		assert_eq!(network_label("***"), "network");
	}

	#[test]
	fn new_network_names_must_already_be_labels() {
		assert_eq!(validate_network("homelab"), Ok("homelab".to_string()));
		assert_eq!(validate_network("Homelab"), Ok("homelab".to_string()));
		assert!(validate_network("my network").is_err());
		assert!(validate_network("").is_err());
		assert!(validate_network(&"a".repeat(MAX_LEN + 1)).is_err());
	}

	#[test]
	fn dedupe_leaves_a_free_name_alone() {
		assert_eq!(dedupe("web", ID, |_| false), "web");
	}

	#[test]
	fn dedupe_walks_the_numeric_suffix() {
		let taken = |n: &str| matches!(n, "web" | "web-1" | "web-2");
		assert_eq!(dedupe("web", ID, taken), "web-3");
	}

	#[test]
	fn dedupe_result_is_always_a_legal_label() {
		// A name already at the limit has to lose characters to make room for
		// the suffix, rather than producing something DNS would reject.
		let base = "a".repeat(MAX_LEN);
		let assigned = dedupe(&base, ID, |n| n == base);

		assert_ne!(assigned, base);
		assert!(validate(&assigned, ID).is_ok(), "got {assigned:?}");
	}

	#[test]
	fn dedupe_terminates_when_every_suffix_is_taken() {
		// Everything is spoken for except the last-resort form, which is keyed
		// to the claimant and so cannot collide.
		let assigned = dedupe("web", ID, |n| n != format!("web-{}", fallback(ID)));
		assert_eq!(assigned, "web-1234abcd");
	}
}
