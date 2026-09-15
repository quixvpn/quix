//! Telling the operating system that `.quix` is ours to answer.
//!
//! Two unrelated mechanisms behind one pair of functions: systemd-resolved on
//! Linux, NRPT on Windows. Neither is essential — the resolver answers on its
//! own port regardless — so every failure here is a warning, not a fatal error.
//!
//! Both are scoped to the zone. Everything outside `.quix` resolves exactly as
//! it did before.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::dns::ZONE;

#[derive(Debug)]
pub enum Error {
	/// There is no system resolver here to register with: not installed, or
	/// installed and not running. Nothing we do changes that.
	///
	/// Unix only. NRPT is part of Windows, so there is nothing there that a
	/// person could have left switched off.
	#[cfg_attr(not(unix), allow(dead_code))]
	NoResolver {
		source: anyhow::Error,
		/// What the user can do about it, in one line, when this platform has
		/// an answer. `None` where quix has no integration to offer at all.
		remedy: Option<&'static str>,
	},
	/// Failed this time. An interface that is still coming up does this, so
	/// another attempt is worth making.
	Transient(anyhow::Error),
}

impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Error::NoResolver { source, .. } | Error::Transient(source) => write!(f, "{source:#}"),
		}
	}
}

/// Points the system resolver at us for `.quix`, and nothing else.
///
/// `iface` is the mesh interface; `server` is where we answer.
pub async fn register(iface: &str, server: SocketAddr) -> Result<(), Error> {
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

	/// What to do about a missing systemd-resolved, in the one line `status` has
	/// room for.
	///
	/// Linux only. The same module is compiled on macOS, where `resolvectl` is
	/// missing for a reason no user can act on: there is no integration there to
	/// enable, which is a limitation rather than a misconfiguration.
	#[cfg(target_os = "linux")]
	const REMEDY: Option<&'static str> = Some(
		"systemd-resolved is not running; enable it with \
		 `sudo systemctl enable --now systemd-resolved`",
	);
	#[cfg(not(target_os = "linux"))]
	const REMEDY: Option<&'static str> = None;

	pub async fn register(iface: &str, server: SocketAddr) -> Result<(), Error> {
		// The one permanent failure here, and the common one: a distro that ships
		// systemd-resolved without enabling it. Everything past this point is a
		// command that could go differently on another attempt.
		available().await.map_err(|source| Error::NoResolver {
			source: source.context("systemd-resolved is not available"),
			remedy: REMEDY,
		})?;

		// resolved takes a port, so we can answer on an unprivileged one.
		// `SocketAddr`'s own formatting is already what it expects: bare for
		// IPv4, bracketed for IPv6.
		//
		// Per-link configuration, deliberately: when the interface goes away so
		// does this, which means a killed daemon leaves nothing stale behind.
		run(&["dns", iface, &server.to_string()])
			.await
			.context("pointing the link at our resolver")
			.map_err(Error::Transient)?;

		// The `~` prefix makes it a *routing* domain: queries for this zone
		// come to us, and nothing else about the system's DNS changes.
		run(&["domain", iface, &format!("~{ZONE}")])
			.await
			.context("claiming the zone")
			.map_err(Error::Transient)?;

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

	pub async fn register(_iface: &str, server: SocketAddr) -> Result<(), Error> {
		// NRPT rules live in the registry and outlast the process, so clear any
		// left by a previous run before adding this one.
		let _ = deregister(_iface).await;

		// NRPT carries no port, which is why the Windows listener is on 53.
		//
		// Nothing here is ever `NoResolver`: NRPT is part of Windows, so there is
		// no equivalent of a resolver that was never switched on. A failure is
		// something about this attempt, and another one may well work.
		powershell(&format!(
			"Add-DnsClientNrptRule -Namespace '.{ZONE}' -NameServers '{}' -Comment '{COMMENT}'",
			server.ip()
		))
		.await
		.context("adding the NRPT rule")
		.map_err(Error::Transient)
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
		// Only a last resort. Shutdown waits for a registration in progress
		// rather than dropping it, and gives up only on one that has hung —
		// killing the client does not undo a change it has already handed to the
		// system, which is why waiting is the real guarantee.
		.kill_on_drop(true)
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
