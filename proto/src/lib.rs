use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
	Ping { peer: String },
	Status,
	CreateNetwork { name: String },
	Invite,
	Join { code: String },
	Leave,
	SetOperator { user: String },
}

/// One peer in the network, as seen from this node.
#[derive(Debug, Serialize, Deserialize)]
pub struct PeerStatus {
	pub id: String,
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
pub enum Response {
	Ok { echo: String },
	Status {
		endpoint_id: String,
		v6: String,
		v4: String,
		network: Option<String>,
		coordinator: bool,
		peers: Vec<PeerStatus>,
		traffic: Traffic,
	},
	Pong { v6: String, v4: String, rtt_ms: Option<f64> },
	Invite { code: String },
	Joined { network_name: Option<String> },
	Left { network_name: Option<String>, coordinator_notified: bool },
	OperatorSet { user: String, uid: u32 },
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
