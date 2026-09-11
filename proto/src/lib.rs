use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
	Ping { peer: String },
	Status,
	CreateNetwork { name: String },
	Invite,
	Join { code: String },
}

/// One peer in the network, as seen from this node.
#[derive(Debug, Serialize, Deserialize)]
pub struct PeerStatus {
	pub id: String,
	pub virtual_ip: String,
	/// Whether a data-plane link to this peer is currently up.
	pub linked: bool,
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
		virtual_ip: String,
		network: Option<String>,
		coordinator: bool,
		peers: Vec<PeerStatus>,
		traffic: Traffic,
	},
	Pong { virtual_ip: String, rtt_ms: Option<f64> },
	Invite { code: String },
	Joined { network_name: Option<String> },
	Error { message: String },
}

/// Name used to identify the daemon's local socket (unix socket path on
/// Unix, named pipe name on Windows).
pub fn socket_name() -> String {
	if let Ok(custom) = std::env::var("QUIX_SOCKET") {
		return custom;
	}
	if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
		return format!("{dir}/quix.sock");
	}
	"quix-daemon".to_string()
}
