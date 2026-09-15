use serde::{Deserialize, Serialize};

/// The version these binaries were built from, resolved by `build.rs`: the git
/// tag in CI, `git describe` locally, `Cargo.toml` as a last resort. Carries no
/// leading `v` — display code adds it.
pub const VERSION: &str = env!("QUIX_VERSION");

/// The same version written the way the release tag is, e.g. `v0.1.3`.
pub const VERSION_TAG: &str = env!("QUIX_VERSION_TAG");

/// The DNS suffix a mesh answers under, and the whole of what the daemon claims
/// from the system resolver.
///
/// Shared rather than spelled out on both sides: the CLI prints names in it and
/// the daemon registers it, and the two disagreeing would mean telling someone a
/// name that resolves nowhere. A network's own zone is a label in front of this
/// — `homelab.quix` — which only the daemon can build, since only it knows how a
/// network name becomes a label.
pub const ZONE: &str = "quix";

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
	/// Remove a peer from the network (coordinator only). `peer` is any name
	/// `status` shows for it: hostname, fallback, either under the zone, or the
	/// full endpoint id.
	Kick { peer: String },
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

/// Whether `.quix` names resolve through the operating system's resolver, or
/// only by asking this node's resolver directly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DnsRegistration {
	/// The system resolver sends `.quix` queries here.
	Registered,
	/// Not registered yet, and the daemon keeps trying, because what failed can
	/// plausibly go differently on another attempt — an interface still
	/// settling, say. Meanwhile names resolve only by asking `fallback`
	/// directly.
	Retrying { fallback: String },
	/// Not registered, and no further attempt will change that until somebody
	/// acts on this machine — most of all, a machine with no system resolver to
	/// register with at all.
	///
	/// `remedy` is that action in one line, when the platform has one to name;
	/// without it the daemon's log is the only explanation. Both fields default,
	/// so a status from a daemon that sends neither still reads as this state
	/// rather than failing to parse.
	Unavailable {
		#[serde(default)]
		fallback: Option<String>,
		#[serde(default)]
		remedy: Option<String>,
	},
	/// The daemon did not say, which only happens reading a status from one that
	/// predates the field.
	///
	/// The default for exactly that reason, and deliberately not `Registered`:
	/// silence is not the same as working, and reading it as working is what let
	/// a machine where `.quix` never resolved at all report nothing wrong. No
	/// daemon ever sends this.
	#[default]
	Unknown,
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
		/// Whether names resolve through the system resolver. Defaulted so a CLI
		/// can still read a status from a daemon that predates it.
		#[serde(default)]
		dns: DnsRegistration,
	},
	Pong { v6: String, v4: String, rtt_ms: Option<f64> },
	Invite {
		code: String,
		/// When the code stops being redeemable, RFC 3339. Shown to whoever
		/// minted it, so they know how long they have to pass it on.
		expires_at: String,
	},
	Joined {
		network_name: Option<String>,
		hostname: Option<String>,
		/// The suffix the new member's names resolve under, e.g. `homelab.quix`,
		/// built the same way `Status` builds it. Sent so the CLI can name what
		/// was just joined without deriving a label of its own.
		///
		/// Defaulted for a daemon that predates it, where empty means "not told"
		/// rather than "no zone".
		#[serde(default)]
		zone: String,
	},
	Left { network_name: Option<String>, coordinator_notified: bool },
	OperatorSet { user: String, uid: u32 },
	HostnameSet { hostname: String, requested: String },
	/// A peer was removed. `notified` is whether it was told, as opposed to
	/// finding out when the links it tries are refused.
	Kicked { name: String, id: String, notified: bool },
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
