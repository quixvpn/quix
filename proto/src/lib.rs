use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
	Ping { peer: String, msg: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
	Ok { echo: String },
	Error { message: String },
}

/// Where the daemon listens for CLI commands.
pub fn socket_path() -> PathBuf {
	if let Ok(custom) = std::env::var("QUIX_SOCKET") {
		return PathBuf::from(custom);
	}
	if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
		return PathBuf::from(dir).join("quix.sock");
	}
	PathBuf::from("/tmp/quix.sock")
}