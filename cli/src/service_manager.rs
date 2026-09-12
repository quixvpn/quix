//! Talking to whichever service manager owns the daemon: systemd on Unix, the
//! Service Control Manager on Windows.
//!
//! This is the one part of the CLI that cannot go through the daemon, because
//! the daemon may be exactly what isn't running. Shared by the `service`
//! subcommand and by `update`, which has to stop the daemon before it can
//! replace the binary on disk.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

pub const NAME: &str = "quixd";

/// How long `start` waits for the daemon to be answerable before giving up.
/// Generous because the daemon waits for a relay before it binds its socket.
const READY_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct State {
	pub installed: bool,
	pub running: bool,
	/// Whether it starts automatically at boot.
	pub enabled: bool,
}

pub fn state() -> State {
	let installed = is_installed();
	State {
		installed,
		running: installed && is_running(),
		enabled: installed && is_enabled(),
	}
}

/// Blocks until the daemon is answering, so a successful `start` means the next
/// command will work rather than racing it.
pub fn wait_until_ready() -> bool {
	let deadline = Instant::now() + READY_TIMEOUT;
	loop {
		if is_answerable() {
			return true;
		}
		if Instant::now() >= deadline {
			return false;
		}
		std::thread::sleep(Duration::from_millis(250));
	}
}

/// Whether the daemon's control socket exists. It is created last, after the
/// relay wait and the TUN setup, so it is the honest readiness signal — the
/// service manager reports "running" well before this.
fn is_answerable() -> bool {
	let name = proto::socket_name();

	#[cfg(windows)]
	{
		let path = if name.starts_with(r"\\") {
			name
		} else {
			format!(r"\\.\pipe\{name}")
		};
		Path::new(&path).exists()
	}
	#[cfg(not(windows))]
	Path::new(&name).exists()
}

// --- systemd ---------------------------------------------------------------

#[cfg(unix)]
fn is_installed() -> bool {
	quiet(Command::new("systemctl").args(["cat", NAME]))
}

#[cfg(unix)]
fn is_running() -> bool {
	quiet(Command::new("systemctl").args(["is-active", "--quiet", NAME]))
}

#[cfg(unix)]
fn is_enabled() -> bool {
	quiet(Command::new("systemctl").args(["is-enabled", "--quiet", NAME]))
}

#[cfg(unix)]
pub fn start() -> Result<()> {
	// `reset-failed` first: repeated crash-restarts trip systemd's start limit,
	// after which a plain start refuses until the counter is cleared.
	let _ = quiet(Command::new("systemctl").args(["reset-failed", NAME]));
	run(Command::new("systemctl").args(["start", NAME]))
}

#[cfg(unix)]
pub fn stop() -> Result<()> {
	run(Command::new("systemctl").args(["stop", NAME]))
}

#[cfg(unix)]
pub fn restart() -> Result<()> {
	let _ = quiet(Command::new("systemctl").args(["reset-failed", NAME]));
	run(Command::new("systemctl").args(["restart", NAME]))
}

#[cfg(unix)]
pub fn enable() -> Result<()> {
	run(Command::new("systemctl").args(["enable", NAME]))
}

#[cfg(unix)]
pub fn disable() -> Result<()> {
	run(Command::new("systemctl").args(["disable", NAME]))
}

// --- Windows service control manager ---------------------------------------

#[cfg(windows)]
fn sc_says(args: &[&str], needle: &str) -> bool {
	Command::new("sc")
		.args(args)
		.output()
		.map(|out| String::from_utf8_lossy(&out.stdout).contains(needle))
		.unwrap_or(false)
}

#[cfg(windows)]
fn is_installed() -> bool {
	quiet(Command::new("sc").args(["query", NAME]))
}

#[cfg(windows)]
fn is_running() -> bool {
	sc_says(&["query", NAME], "RUNNING")
}

#[cfg(windows)]
fn is_enabled() -> bool {
	sc_says(&["qc", NAME], "AUTO_START")
}

#[cfg(windows)]
pub fn start() -> Result<()> {
	run(Command::new("sc").args(["start", NAME]))
}

#[cfg(windows)]
pub fn stop() -> Result<()> {
	run(Command::new("sc").args(["stop", NAME]))?;

	// `sc stop` only asks; wait for the process to actually go away.
	for _ in 0..40 {
		if !is_running() {
			return Ok(());
		}
		std::thread::sleep(Duration::from_millis(500));
	}
	anyhow::bail!("the service did not stop within 20s")
}

#[cfg(windows)]
pub fn restart() -> Result<()> {
	if is_running() {
		stop()?;
	}
	start()
}

#[cfg(windows)]
pub fn enable() -> Result<()> {
	// sc.exe wants the value as its own argument: `start= auto`.
	run(Command::new("sc").args(["config", NAME, "start=", "auto"]))
}

#[cfg(windows)]
pub fn disable() -> Result<()> {
	run(Command::new("sc").args(["config", NAME, "start=", "demand"]))
}

// --- running the tools -----------------------------------------------------

fn quiet(command: &mut Command) -> bool {
	command
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.status()
		.map(|s| s.success())
		.unwrap_or(false)
}

fn run(command: &mut Command) -> Result<()> {
	let output = command.output().context("running the service manager")?;
	if output.status.success() {
		return Ok(());
	}

	let mut message = String::from_utf8_lossy(&output.stderr).trim().to_string();
	if message.is_empty() {
		message = String::from_utf8_lossy(&output.stdout).trim().to_string();
	}
	if message.is_empty() {
		message = format!("exited with {}", output.status);
	}

	// Both managers refuse without elevation, and their own wording is unclear.
	if message.contains("Access is denied")
		|| message.contains("access denied")
		|| message.to_lowercase().contains("authentication")
	{
		anyhow::bail!("{message}\n(this needs root — try again with sudo)");
	}
	anyhow::bail!("{message}")
}
