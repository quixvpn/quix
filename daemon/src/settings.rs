use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Daemon settings that outlive any one network, kept apart from `network.json`
/// so leaving a network doesn't discard who is allowed to run commands.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Settings {
	/// The local user permitted to run mutating commands without root.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub operator_uid: Option<u32>,
	/// Kept only so `quix status` can show a name rather than a bare number.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub operator_name: Option<String>,
}

fn path() -> Result<PathBuf> {
	if let Ok(custom) = std::env::var("QUIX_SETTINGS_PATH") {
		return Ok(PathBuf::from(custom));
	}
	let config_dir = dirs::config_dir().context("resolve config dir")?;
	Ok(config_dir.join("quix").join("settings.json"))
}

impl Settings {
	pub fn load() -> Result<Self> {
		let path = path()?;
		match std::fs::read_to_string(&path) {
			Ok(data) => {
				serde_json::from_str(&data).with_context(|| format!("parse {}", path.display()))
			}
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
			Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
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
}
