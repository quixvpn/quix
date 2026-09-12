//! Telling the operating system that `.quix` is ours to answer.
//!
//! Two unrelated mechanisms behind one pair of functions: systemd-resolved on
//! Linux, NRPT on Windows. Neither is essential — the resolver answers on its
//! own port regardless — so every failure here is a warning, not a fatal error.
//!
//! Both are scoped to the zone. Everything outside `.quix` resolves exactly as
//! it did before.

use std::net::IpAddr;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::dns::ZONE;

/// Points the system resolver at us for `.quix`, and nothing else.
///
/// `iface` is the mesh interface; `server` is the address we answer on.
pub async fn register(iface: &str, server: IpAddr) -> Result<()> {
	platform::register(iface, server).await
}

/// Undoes [`register`]. Safe to call when nothing was registered.
pub async fn deregister(iface: &str) -> Result<()> {
	platform::deregister(iface).await
}

// --- systemd-resolved ------------------------------------------------------

#[cfg(unix)]
mod platform {
	use super::*;

	pub async fn register(iface: &str, server: IpAddr) -> Result<()> {
		available()
			.await
			.context("systemd-resolved is not available")?;

		// Per-link configuration, deliberately: when the interface goes away so
		// does this, which means a killed daemon leaves nothing stale behind.
		run(&["dns", iface, &server.to_string()])
			.await
			.context("pointing the link at our resolver")?;

		// The `~` prefix makes it a *routing* domain: queries for this zone
		// come to us, and nothing else about the system's DNS changes.
		run(&["domain", iface, &format!("~{ZONE}")])
			.await
			.context("claiming the zone")?;

		Ok(())
	}

	pub async fn deregister(iface: &str) -> Result<()> {
		// Drops every per-link setting we made in one call. Harmless if the
		// link is already gone.
		run(&["revert", iface]).await
	}

	async fn available() -> Result<()> {
		use std::process::Stdio;

		let ok = Command::new("resolvectl")
			.arg("status")
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()
			.await
			.map(|s| s.success())
			.unwrap_or(false);

		match ok {
			true => Ok(()),
			false => anyhow::bail!("resolvectl is missing or systemd-resolved is not running"),
		}
	}

	async fn run(args: &[&str]) -> Result<()> {
		super::run("resolvectl", args).await
	}
}

// --- Windows NRPT ----------------------------------------------------------

#[cfg(windows)]
mod platform {
	use super::*;

	/// Tagged so we only ever remove rules we created.
	const COMMENT: &str = "quix mesh resolver";

	pub async fn register(_iface: &str, server: IpAddr) -> Result<()> {
		// NRPT rules live in the registry and outlast the process, so clear any
		// left by a previous run before adding this one.
		let _ = deregister(_iface).await;

		powershell(&format!(
			"Add-DnsClientNrptRule -Namespace '.{ZONE}' -NameServers '{server}' -Comment '{COMMENT}'"
		))
		.await
		.context("adding the NRPT rule")
	}

	pub async fn deregister(_iface: &str) -> Result<()> {
		powershell(&format!(
			"Get-DnsClientNrptRule | Where-Object {{ $_.Comment -eq '{COMMENT}' }} | \
			 Remove-DnsClientNrptRule -Force -ErrorAction SilentlyContinue"
		))
		.await
		.context("removing the NRPT rule")
	}

	async fn powershell(script: &str) -> Result<()> {
		super::run(
			"powershell",
			&["-NoProfile", "-NonInteractive", "-Command", script],
		)
		.await
	}
}

async fn run(program: &str, args: &[&str]) -> Result<()> {
	let output = Command::new(program)
		.args(args)
		.output()
		.await
		.with_context(|| format!("running {program}"))?;

	if output.status.success() {
		return Ok(());
	}

	let mut message = String::from_utf8_lossy(&output.stderr).trim().to_string();
	if message.is_empty() {
		message = String::from_utf8_lossy(&output.stdout).trim().to_string();
	}
	anyhow::bail!("{program} {}: {message}", args.join(" "));
}
