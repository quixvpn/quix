use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use sha2::{Digest, Sha256};

use crate::service_manager as manager;

const REPO: &str = "quixvpn/quix";

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

/// What a run of `update` is going to do, decided from the versions alone —
/// before anything is downloaded, prompted for or touched.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
	/// `--check`: report what is available and stop.
	Report,
	/// Already on the latest version, and no `--force`: say so and stop.
	UpToDate,
	/// Download the release and install it.
	Install,
}

/// Any difference counts, not only a newer release: a local build reports a
/// `git describe` version, and updating it means installing the release.
fn plan(current: &str, latest: &str, check: bool, force: bool) -> Plan {
	if check {
		return Plan::Report;
	}
	if current == latest && !force {
		return Plan::UpToDate;
	}
	Plan::Install
}

pub async fn run(args: UpdateArgs) -> Result<()> {
	let current = proto::VERSION;
	let tag = latest_release().await?;
	let latest = tag.trim_start_matches('v');

	println!("installed {current}, latest {latest}");

	match plan(current, latest, args.check, args.force) {
		Plan::Report => return Ok(()),
		Plan::UpToDate => {
			println!("already on the latest version (v{latest})");
			return Ok(());
		}
		Plan::Install => {}
	}

	// Before the privilege check, so a machine without the service is told why
	// rather than prompted first.
	if !manager::state().installed {
		anyhow::bail!(
			"the {} service is not installed, so this copy was not set up by the \
			 installer — run the installer instead (see the README)",
			manager::NAME
		);
	}
	ensure_privileged()?;

	let platform = platform()?;
	let windows = cfg!(windows);
	let archive = archive_name(platform);

	// The archive the installers themselves use, verified before a byte of it is
	// written anywhere: a failed or tampered download changes nothing.
	println!("downloading {archive}");
	let bytes = download_verified(&tag, &archive).await?;

	let stage = Stage::new()?;
	let archive_path = stage.path().join(&archive);
	std::fs::write(&archive_path, &bytes).context("saving the release archive")?;
	run_quietly(extract_command(&archive_path, stage.path(), windows))
		.context("extracting the release archive")?;

	let package = stage.path().join(format!("quix-{platform}"));
	check_package(&package, windows)?;

	// Everything from here — swapping the binaries, configuring the service,
	// restarting it and waiting for it — belongs to the installer, so an update
	// ends up exactly where an install would.
	println!("running the release installer");
	let status = installer_command(&package, &install_dir()?, windows)
		.status()
		.context("starting the installer")?;

	if !status.success() {
		let code = status
			.code()
			.map_or_else(|| status.to_string(), |code| format!("exit {code}"));
		// Its own output has already said why.
		anyhow::bail!("the installer failed ({code})");
	}
	Ok(())
}

/// Installing writes the install directory and the service configuration, so it
/// needs root. Checked only once there is something to install.
#[cfg(unix)]
fn ensure_privileged() -> Result<()> {
	// SAFETY: geteuid has no preconditions and cannot fail.
	if unsafe { libc::geteuid() } != 0 {
		anyhow::bail!("updating installs a system service — run it as root: sudo quix update");
	}
	Ok(())
}

/// The same on Windows, where it means Administrator. Re-runs this exact command
/// elevated and exits with its result; the elevated copy repeats the version
/// check, which is one cheap request.
#[cfg(windows)]
fn ensure_privileged() -> Result<()> {
	if !crate::elevate::is_elevated() {
		crate::elevate::relaunch();
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

/// The release archive for a platform — the one the installers download,
/// carrying both binaries, the installer and, on Linux, the systemd unit.
fn archive_name(platform: &str) -> String {
	match platform.starts_with("windows") {
		true => format!("quix-{platform}.zip"),
		false => format!("quix-{platform}.tar.gz"),
	}
}

/// Pinned to the tag that was just reported rather than `/latest`, so a release
/// published mid-update cannot change what gets installed.
fn release_url(tag: &str, asset: &str) -> String {
	format!("https://github.com/{REPO}/releases/download/{tag}/{asset}")
}

/// Downloads an asset and its checksum, returning the bytes only if they match.
async fn download_verified(tag: &str, asset: &str) -> Result<Vec<u8>> {
	let bytes = get(&release_url(tag, asset)).await?;
	let sums = get(&release_url(tag, &format!("{asset}.sha256"))).await?;

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

/// A temporary directory that is removed when dropped, so it goes on every
/// return path, including the errors.
struct Stage(PathBuf);

impl Stage {
	fn new() -> Result<Self> {
		let path = std::env::temp_dir().join(format!("quix-update-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&path);
		std::fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
		Ok(Self(path))
	}

	fn path(&self) -> &Path {
		&self.0
	}
}

impl Drop for Stage {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

/// A program from the Windows system directory, by absolute path, so nothing
/// earlier on PATH is picked up in its place.
fn system32(parts: &[&str]) -> PathBuf {
	let mut path = std::env::var_os("SystemRoot")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
	path.push("System32");
	for part in parts {
		path.push(part);
	}
	path
}

/// Unpacks the release archive with the system's own `tar`: the one the Linux
/// installer already relies on, and on Windows the inbox bsdtar, which reads
/// zip — Git's GNU tar, often earlier on PATH, does not.
fn extract_command(archive: &Path, into: &Path, windows: bool) -> Command {
	let (program, flags) = match windows {
		true => (system32(&["tar.exe"]), "-xf"),
		false => (PathBuf::from("tar"), "-xzf"),
	};

	let mut command = Command::new(program);
	command.arg(flags).arg(archive).arg("-C").arg(into);
	command
}

/// Runs a helper tool, showing its output only if it fails.
fn run_quietly(mut command: Command) -> Result<()> {
	let program = command.get_program().to_string_lossy().into_owned();
	let output = command.output().with_context(|| format!("running {program}"))?;
	if output.status.success() {
		return Ok(());
	}

	let stderr = String::from_utf8_lossy(&output.stderr);
	anyhow::bail!("{program} failed ({}): {}", output.status, stderr.trim())
}

/// Checks the extracted package holds everything its installer will need,
/// naming the first thing that is missing.
fn check_package(dir: &Path, windows: bool) -> Result<()> {
	let required: &[&str] = match windows {
		true => &["quix.exe", "quixd.exe", "install.ps1"],
		false => &["quix", "quixd", "quixd.service", "install.sh"],
	};

	for file in required {
		if !dir.join(file).is_file() {
			anyhow::bail!("{file} is missing from the release package");
		}
	}
	Ok(())
}

/// The release's own installer, pointed at the extracted package and at the
/// directory this copy is installed in, keeping the operator already chosen.
fn installer_command(package: &Path, install_dir: &Path, windows: bool) -> Command {
	if windows {
		let mut command = Command::new(system32(&["WindowsPowerShell", "v1.0", "powershell.exe"]));
		command
			.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
			.arg(package.join("install.ps1"))
			.arg("-From")
			.arg(package)
			.arg("-InstallDir")
			.arg(install_dir)
			.arg("-KeepOperator");
		return command;
	}

	let mut command = Command::new("bash");
	command
		.arg(package.join("install.sh"))
		.arg("--from")
		.arg(package)
		.arg("--keep-operator")
		.env("INSTALL_DIR", install_dir);
	command
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

fn platform() -> Result<&'static str> {
	Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
		("linux", "x86_64") => "linux-x86_64",
		("linux", "aarch64") => "linux-aarch64",
		("windows", "x86_64") => "windows-x86_64",
		(os, arch) => anyhow::bail!("no prebuilt release for {os}-{arch}; build from source"),
	})
}

fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::ffi::OsStr;

	#[test]
	fn archive_names_match_what_ci_publishes() {
		assert_eq!(archive_name("linux-x86_64"), "quix-linux-x86_64.tar.gz");
		assert_eq!(archive_name("linux-aarch64"), "quix-linux-aarch64.tar.gz");
		assert_eq!(archive_name("windows-x86_64"), "quix-windows-x86_64.zip");
	}

	#[test]
	fn downloads_are_pinned_to_the_reported_tag() {
		// Not `/latest`: a release published mid-update must not change what
		// gets installed.
		assert_eq!(
			release_url("v0.2.0", "quix-linux-x86_64.tar.gz"),
			"https://github.com/quixvpn/quix/releases/download/v0.2.0/quix-linux-x86_64.tar.gz"
		);
		assert_eq!(
			release_url("v0.2.0", "quix-linux-x86_64.tar.gz.sha256"),
			"https://github.com/quixvpn/quix/releases/download/v0.2.0/quix-linux-x86_64.tar.gz.sha256"
		);
	}

	const LINUX_PACKAGE: &[&str] = &["quix", "quixd", "quixd.service", "install.sh"];
	const WINDOWS_PACKAGE: &[&str] = &["quix.exe", "quixd.exe", "install.ps1"];

	/// A throwaway directory holding empty files with the given names.
	struct TestDir(PathBuf);

	impl TestDir {
		fn with(name: &str, files: &[&str]) -> Self {
			let dir = std::env::temp_dir().join(format!("quix-update-test-{}-{name}", std::process::id()));
			let _ = std::fs::remove_dir_all(&dir);
			std::fs::create_dir_all(&dir).unwrap();
			for file in files {
				std::fs::write(dir.join(file), b"").unwrap();
			}
			Self(dir)
		}
	}

	impl Drop for TestDir {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}

	#[test]
	fn a_complete_package_passes() {
		let linux = TestDir::with("linux-complete", LINUX_PACKAGE);
		assert!(check_package(&linux.0, false).is_ok());

		let windows = TestDir::with("windows-complete", WINDOWS_PACKAGE);
		assert!(check_package(&windows.0, true).is_ok());
	}

	#[test]
	fn a_package_missing_anything_is_refused_by_name() {
		for &(windows, required) in &[(false, LINUX_PACKAGE), (true, WINDOWS_PACKAGE)] {
			for missing in required {
				let present: Vec<&str> = required.iter().copied().filter(|file| file != missing).collect();
				let dir = TestDir::with(&format!("missing-{missing}"), &present);

				let refused = check_package(&dir.0, windows).unwrap_err().to_string();

				assert!(refused.contains(missing), "{refused}");
			}
		}
	}

	fn args_of(command: &Command) -> Vec<String> {
		command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect()
	}

	#[test]
	fn linux_hands_off_to_the_packaged_install_sh() {
		let package = Path::new("/tmp/quix-update-1/quix-linux-x86_64");
		let command = installer_command(package, Path::new("/usr/local/bin"), false);

		assert_eq!(command.get_program(), "bash");
		let script = package.join("install.sh").display().to_string();
		let from = package.display().to_string();
		assert_eq!(args_of(&command), vec![script.as_str(), "--from", from.as_str(), "--keep-operator"]);

		// Where this copy is installed, so the update lands on top of it.
		let install_dir = command
			.get_envs()
			.find(|(key, _)| key.eq_ignore_ascii_case("INSTALL_DIR"))
			.and_then(|(_, value)| value);
		assert_eq!(install_dir, Some(OsStr::new("/usr/local/bin")));
	}

	#[test]
	fn windows_hands_off_to_the_packaged_install_ps1() {
		let package = Path::new(r"C:\Temp\quix-update-1\quix-windows-x86_64");
		let install_dir = Path::new(r"C:\Program Files\quix");
		let command = installer_command(package, install_dir, true);

		let program = command.get_program().to_string_lossy().to_lowercase();
		assert!(program.ends_with("powershell.exe") && program.contains("system32"), "{program}");

		let script = package.join("install.ps1").display().to_string();
		let from = package.display().to_string();
		let target = install_dir.display().to_string();
		assert_eq!(
			args_of(&command),
			vec![
				"-NoProfile",
				"-ExecutionPolicy",
				"Bypass",
				"-File",
				script.as_str(),
				"-From",
				from.as_str(),
				"-InstallDir",
				target.as_str(),
				"-KeepOperator",
			]
		);
	}

	#[test]
	fn windows_extracts_with_the_inbox_tar_not_whatever_is_on_path() {
		// Git's GNU tar is often first on PATH, and it cannot read zip.
		let command = extract_command(Path::new("quix.zip"), Path::new("out"), true);

		let program = command.get_program().to_string_lossy().to_lowercase();
		assert!(program.ends_with("tar.exe") && program.contains("system32"), "{program}");
		assert_eq!(args_of(&command), vec!["-xf", "quix.zip", "-C", "out"]);
	}

	#[test]
	fn linux_extracts_the_tarball_with_tar() {
		let command = extract_command(Path::new("quix.tar.gz"), Path::new("out"), false);

		assert_eq!(command.get_program(), "tar");
		assert_eq!(args_of(&command), vec!["-xzf", "quix.tar.gz", "-C", "out"]);
	}

	#[test]
	fn check_only_reports_whatever_the_versions() {
		assert_eq!(plan("0.2.0", "0.2.0", true, false), Plan::Report);
		assert_eq!(plan("0.1.0", "0.2.0", true, false), Plan::Report);
		assert_eq!(plan("0.1.0", "0.2.0", true, true), Plan::Report);
	}

	#[test]
	fn the_latest_version_is_left_alone() {
		// Nothing downloaded, nothing reinstalled, no service restart.
		assert_eq!(plan("0.2.0", "0.2.0", false, false), Plan::UpToDate);
	}

	#[test]
	fn force_reinstalls_the_same_version() {
		assert_eq!(plan("0.2.0", "0.2.0", false, true), Plan::Install);
	}

	#[test]
	fn any_other_version_is_installed() {
		assert_eq!(plan("0.1.0", "0.2.0", false, false), Plan::Install);
		// A local build reports its `git describe` version; updating it means
		// installing the release, not keeping the build.
		assert_eq!(plan("0.2.0-3-gabc1234", "0.2.0", false, false), Plan::Install);
	}

	#[test]
	fn hex_matches_the_sha256sum_format() {
		assert_eq!(hex(&Sha256::digest(b"")),
			"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
	}
}
