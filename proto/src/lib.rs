use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
	Ping { peer: String, msg: String },
	Status,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
	Ok { echo: String },
	Status { endpoint_id: String, peer_count: u64 },
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