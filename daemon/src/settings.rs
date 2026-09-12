use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Daemon settings that outlive any one network, kept apart from `network.json`
/// so leaving a network doesn't discard who is allowed to run commands.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Settings {
	/// The local user permitted to run mutating commands without root. Unix: a
	/// uid has no meaning on Windows, which uses `operator_sid` instead.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub operator_uid: Option<u32>,
	/// The same thing on Windows: the user's SID, in canonical string form.
	///
	/// A SID rather than a name because names are renameable, and rather than a
	/// uid because there is no such thing here. Unlike a uid it is also never
	/// recycled — deleting and recreating an account produces a new SID, so
	/// authority cannot silently pass to a different person.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub operator_sid: Option<String>,
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

#[cfg(test)]
mod tests {
	use super::*;

	/// The Windows installer writes this file from PowerShell, so the two sides
	/// agree on a shape neither compiler checks. This reproduces that shape
	/// exactly as `ConvertTo-Json` emits it — the two-space indentation and the
	/// escaped backslash in the account name — with invented values.
	#[test]
	fn the_windows_installers_settings_file_parses() {
		let written = r#"{
    "operator_sid":  "S-1-5-21-1111111111-2222222222-3333333333-1001",
    "operator_name":  "TESTBOX\\testuser"
}"#;

		let settings: Settings = serde_json::from_str(written).expect("installer output must load");

		assert_eq!(
			settings.operator_sid.as_deref(),
			Some("S-1-5-21-1111111111-2222222222-3333333333-1001")
		);
		assert_eq!(settings.operator_name.as_deref(), Some(r"TESTBOX\testuser"));
		assert_eq!(settings.operator_uid, None, "a SID is not a uid");
	}

	#[test]
	fn a_settings_file_from_before_the_windows_operator_still_loads() {
		let stored = r#"{"operator_uid": 1000, "operator_name": "alice"}"#;

		let settings: Settings = serde_json::from_str(stored).expect("must load");

		assert_eq!(settings.operator_uid, Some(1000));
		assert_eq!(settings.operator_sid, None);
	}

	#[test]
	fn an_empty_file_means_no_operator_rather_than_a_failure() {
		let settings: Settings = serde_json::from_str("{}").expect("must load");
		assert_eq!(settings.operator_uid, None);
		assert_eq!(settings.operator_sid, None);
	}
}
