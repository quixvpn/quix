use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use sha2::{Digest, Sha256};

const REPO: &str = "quixvpn/quix";
const SERVICE: &str = "quixd";
const BINARIES: [&str; 2] = ["quix", "quixd"];

/// Download and install the latest release
#[derive(Args)]
pub struct UpdateArgs {
	/// Report what's available without installing it
	#[arg(long)]
	pub check: bool,
	/// Reinstall even when already on the latest version
	#[arg(long)]
	pub force: bool,
}

pub async fn run(args: UpdateArgs) -> Result<()> {
	let current = proto::VERSION;
	let release = latest_release().await?;
	let latest = release.trim_start_matches('v');

	println!("installed {current}, latest {latest}");

	if args.check {
		return Ok(());
	}
	if latest == current && !args.force {
		println!("already up to date");
		return Ok(());
	}

	let install_dir = install_dir()?;
	writable(&install_dir)?;

	// Fetch and verify everything before touching the installed copies, so a
	// failed download can't leave a half-updated system.
	let platform = platform()?;
	let mut staged = Vec::new();
	for name in BINARIES {
		let asset = asset_name(name, platform);
		println!("downloading {asset}");
		staged.push((name, download_verified(&release, &asset).await?));
	}

	let was_running = service_is_running();
	if was_running {
		println!("stopping {SERVICE}");
		stop_service().context("could not stop the service")?;
	}

	for (name, bytes) in &staged {
		replace(&install_dir, name, bytes)
			.with_context(|| format!("installing {name}"))?;
	}

	if was_running {
		println!("starting {SERVICE}");
		start_service().context("binaries were updated, but the service did not restart")?;
	}

	println!("updated to {latest}");
	if !was_running {
		println!("note: the {SERVICE} service was not running, so it was left alone");
	}
	Ok(())
}

/// The tag of the newest published release.
async fn latest_release() -> Result<String> {
	let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
	let body: serde_json::Value = reqwest::Client::builder()
		// GitHub rejects API requests without one.
		.user_agent(format!("quix/{}", proto::VERSION))
		.timeout(Duration::from_secs(30))
		.build()?
		.get(&url)
		.send()
		.await
		.context("asking GitHub for the latest release")?
		.error_for_status()
		.context("no published release found")?
		.json()
		.await?;

	body["tag_name"]
		.as_str()
		.map(str::to_string)
		.context("release has no tag")
}

/// Downloads an asset and its checksum, returning the bytes only if they match.
async fn download_verified(tag: &str, asset: &str) -> Result<Vec<u8>> {
	let base = format!("https://github.com/{REPO}/releases/download/{tag}");

	let bytes = get(&format!("{base}/{asset}")).await?;
	let sums = get(&format!("{base}/{asset}.sha256")).await?;

	let expected = String::from_utf8_lossy(&sums)
		.split_whitespace()
		.next()
		.context("malformed checksum file")?
		.to_lowercase();
	let actual = hex(&Sha256::digest(&bytes));

	if expected != actual {
		anyhow::bail!("checksum mismatch for {asset} — refusing to install");
	}
	Ok(bytes)
}

async fn get(url: &str) -> Result<Vec<u8>> {
	let response = reqwest::Client::builder()
		.user_agent(format!("quix/{}", proto::VERSION))
		.timeout(Duration::from_secs(300))
		.build()?
		.get(url)
		.send()
		.await
		.with_context(|| format!("downloading {url}"))?
		.error_for_status()
		.with_context(|| format!("downloading {url}"))?;

	Ok(response.bytes().await?.to_vec())
}

/// Swaps one binary, moving the old one aside first.
///
/// A running executable cannot be overwritten on Windows, and replacing one
/// in place on Unix would be seen by anything mid-exec. Renaming works on both:
/// the old file keeps its identity for whoever still holds it, and the new one
/// takes the name.
fn replace(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
	let target = dir.join(exe(name));
	let old = dir.join(format!("{}.old", exe(name)));

	let _ = std::fs::remove_file(&old);
	if target.exists() {
		std::fs::rename(&target, &old).context("moving the old binary aside")?;
	}

	if let Err(e) = std::fs::write(&target, bytes) {
		// Put it back rather than leaving nothing installed.
		let _ = std::fs::rename(&old, &target);
		return Err(e).context("writing the new binary");
	}

	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
	}

	// Best-effort: on Windows this fails while the old binary is still mapped,
	// and it gets cleaned up by the next update.
	let _ = std::fs::remove_file(&old);
	Ok(())
}

/// Where the binaries live, taken from the running executable so an update
/// lands wherever this copy was installed.
fn install_dir() -> Result<PathBuf> {
	let exe = std::env::current_exe().context("locating the running executable")?;
	Ok(exe
		.parent()
		.context("executable has no parent directory")?
		.to_path_buf())
}

fn writable(dir: &Path) -> Result<()> {
	let probe = dir.join(".quix-update-probe");
	std::fs::write(&probe, b"")
		.with_context(|| format!("cannot write to {} — try again with sudo", dir.display()))?;
	let _ = std::fs::remove_file(&probe);
	Ok(())
}

fn platform() -> Result<&'static str> {
	Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
		("linux", "x86_64") => "linux-x86_64",
		("linux", "aarch64") => "linux-aarch64",
		("windows", "x86_64") => "windows-x86_64",
		(os, arch) => anyhow::bail!("no prebuilt release for {os}-{arch}; build from source"),
	})
}

fn asset_name(binary: &str, platform: &str) -> String {
	format!("{binary}-{platform}{}", std::env::consts::EXE_SUFFIX)
}

fn exe(name: &str) -> String {
	format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(unix)]
fn service_is_running() -> bool {
	Command::new("systemctl")
		.args(["is-active", "--quiet", SERVICE])
		.status()
		.map(|s| s.success())
		.unwrap_or(false)
}

#[cfg(unix)]
fn stop_service() -> Result<()> {
	run_ok(Command::new("systemctl").args(["stop", SERVICE]))
}

#[cfg(unix)]
fn start_service() -> Result<()> {
	run_ok(Command::new("systemctl").args(["start", SERVICE]))
}

#[cfg(windows)]
fn service_is_running() -> bool {
	Command::new("sc")
		.args(["query", SERVICE])
		.output()
		.map(|out| String::from_utf8_lossy(&out.stdout).contains("RUNNING"))
		.unwrap_or(false)
}

#[cfg(windows)]
fn stop_service() -> Result<()> {
	run_ok(Command::new("sc").args(["stop", SERVICE]))?;

	// `sc stop` only asks; the file stays locked until the process exits, so
	// wait for it rather than racing the replacement.
	for _ in 0..40 {
		if !service_is_running() {
			// The handle can outlive the RUNNING state by a moment.
			std::thread::sleep(Duration::from_millis(500));
			return Ok(());
		}
		std::thread::sleep(Duration::from_millis(500));
	}
	anyhow::bail!("the service did not stop within 20s")
}

#[cfg(windows)]
fn start_service() -> Result<()> {
	run_ok(Command::new("sc").args(["start", SERVICE]))
}

fn run_ok(command: &mut Command) -> Result<()> {
	let output = command.output().context("running the service manager")?;
	if output.status.success() {
		return Ok(());
	}
	let mut message = String::from_utf8_lossy(&output.stderr).trim().to_string();
	if message.is_empty() {
		message = String::from_utf8_lossy(&output.stdout).trim().to_string();
	}
	anyhow::bail!("{message}")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn asset_names_match_what_ci_publishes() {
		if cfg!(windows) {
			assert_eq!(asset_name("quixd", "windows-x86_64"), "quixd-windows-x86_64.exe");
		} else {
			assert_eq!(asset_name("quixd", "linux-aarch64"), "quixd-linux-aarch64");
		}
	}

	#[test]
	fn hex_matches_the_sha256sum_format() {
		assert_eq!(hex(&Sha256::digest(b"")),
			"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
	}
}
