use serde::{Deserialize, Serialize};

/// The version these binaries were built from, resolved by `build.rs`: the git
/// tag in CI, `git describe` locally, `Cargo.toml` as a last resort. Carries no
/// leading `v` — display code adds it.
pub const VERSION: &str = env!("QUIX_VERSION");

/// The same version written the way the release tag is, e.g. `v0.1.3`.
pub const VERSION_TAG: &str = env!("QUIX_VERSION_TAG");

/// How long an invite stays redeemable when `--expires` is not given.
///
/// Short on purpose: a code is meant to be handed over and used, and the window
/// it is valid for is the window someone else can use it in.
pub const DEFAULT_INVITE_TTL: u64 = 5 * 60;

/// The longest window an invite may be minted for. An invite that outlives the
/// conversation it was shared in is the problem expiry exists to solve.
pub const MAX_INVITE_TTL: u64 = 30 * 24 * 60 * 60;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
	Ping { peer: String },
	Status,
	CreateNetwork { name: String, hostname: Option<String> },
	Invite {
		/// How long the code stays redeemable. Defaulted rather than required so
		/// a CLI from before invites expired still talks to a newer daemon —
		/// and gets the short default rather than an invite that never dies.
		#[serde(default = "default_invite_ttl")]
		ttl_secs: u64,
	},
	Join { code: String, hostname: Option<String> },
	Leave,
	SetOperator { user: String },
	SetHostname { hostname: String, force: bool },
}

fn default_invite_ttl() -> u64 {
	DEFAULT_INVITE_TTL
}

/// One peer in the network, as seen from this node.
#[derive(Debug, Serialize, Deserialize)]
pub struct PeerStatus {
	pub id: String,
	/// Hostname if set, otherwise the peer's 8-character fallback identifier.
	/// Always resolvable under `.quix`.
	pub name: String,
	/// Whether `name` is a real hostname rather than the fallback.
	pub named: bool,
	/// Primary overlay address.
	pub v6: String,
	/// Compatibility address, for applications that cannot speak IPv6.
	pub v4: String,
	/// Whether a data-plane link to this peer is currently up.
	pub linked: bool,
	/// Bytes this link will carry in one datagram, when linked. Below the TUN
	/// MTU means full-size packets are being dropped.
	pub datagram_max: Option<u32>,
}

/// Per-hop packet counters, in the order a packet visits them.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Traffic {
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

#[derive(Debug, Serialize, Deserialize)]
// Status carries far more than the other variants, but exactly one Response is
// built and consumed per command, so boxing it would buy an allocation and an
// indirection rather than save anything.
#[allow(clippy::large_enum_variant)]
pub enum Response {
	Ok { echo: String },
	Status {
		endpoint_id: String,
		/// This node's own hostname, or its fallback identifier.
		name: String,
		named: bool,
		v6: String,
		v4: String,
		network: Option<String>,
		coordinator: bool,
		peers: Vec<PeerStatus>,
		/// Suffix names resolve under, e.g. `homelab.quix`.
		zone: String,
		traffic: Traffic,
		/// Hostnames a roster push tried to rebind and we refused.
		conflicts: Vec<String>,
	},
	Pong { v6: String, v4: String, rtt_ms: Option<f64> },
	Invite {
		code: String,
		/// When the code stops being redeemable, RFC 3339. Shown to whoever
		/// minted it, so they know how long they have to pass it on.
		expires_at: String,
	},
	Joined { network_name: Option<String>, hostname: Option<String> },
	Left { network_name: Option<String>, coordinator_notified: bool },
	OperatorSet { user: String, uid: u32 },
	HostnameSet { hostname: String, requested: String },
	/// The caller may not run this command as who they are. Distinct from
	/// `Error` so a client can react to it — on Windows the CLI retries the
	/// command elevated rather than making the user work out why it failed.
	Unauthorized { message: String },
	Error { message: String },
}

/// Name used to identify the daemon's local socket (unix socket path on Unix,
/// named pipe name on Windows).
///
/// The Unix default is a system path rather than a per-user one: the daemon
/// runs as a system service, so the CLI has to find it without knowing which
/// user started it. Override with QUIX_SOCKET to run a daemon out of a build
/// tree, or several side by side.
pub fn socket_name() -> String {
	if let Ok(custom) = std::env::var("QUIX_SOCKET") {
		return custom;
	}
	if cfg!(windows) {
		return "quix-daemon".to_string();
	}
	"/run/quix/quixd.sock".to_string()
}
