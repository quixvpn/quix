use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use proto::filename::{self, Platform};
use proto::frame::{self, Frame, FrameReader};
use proto::{IncomingOffer, Request, Response};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::{dest, format, space, Ended};
use crate::commands::client;

/// Accept an offer by id and save the file
#[derive(Args)]
pub struct AcceptArgs {
	/// The offer's id, as `quix file list` shows it
	pub id: String,
	/// Save into the current directory instead of Downloads
	#[arg(long)]
	pub here: bool,
}

/// Reject an offer by id
#[derive(Args)]
pub struct RejectArgs {
	/// The offer's id, as `quix file list` shows it
	pub id: String,
}

pub async fn run_accept(args: AcceptArgs) -> Result<()> {
	let dest = dest::for_caller(args.here)?;
	if let Some(note) = &dest.note {
		println!("{note}");
	}
	let saved = accept(&args.id, &dest.dir).await?;
	println!("saved {}", saved.display());
	Ok(())
}

pub async fn run_reject(args: RejectArgs) -> Result<()> {
	reject(&args.id)
		.await
		.map(|(name, from)| println!("rejected {name} from {from}"))
}

pub async fn reject(id: &str) -> Result<(String, String)> {
	match client::send_as_caller(Request::FileReject { id: id.to_string() }).await? {
		Response::FileRejected { name, from, .. } => Ok((name, from)),
		Response::Error { message } => anyhow::bail!("reject failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}

/// The offers waiting here, and this node's own.
pub async fn offers() -> Result<(Vec<IncomingOffer>, Vec<proto::OutgoingOffer>)> {
	match client::send_as_caller(Request::FileList).await? {
		Response::Files { incoming, outgoing } => Ok((incoming, outgoing)),
		Response::Error { message } => anyhow::bail!("list failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}

/// Accepts one offer and saves it into `dir`, returning where it landed.
///
/// Everything that can fail without the sender is tried first, while the offer
/// is still untouched: the name, the free space, and whether this user can
/// create a file in `dir` at all.
pub async fn accept(id: &str, dir: &Path) -> Result<PathBuf> {
	// Created before the daemon is asked for anything, so a directory this user
	// may not write to fails with the operating system's error without the
	// daemon ever being contacted, and the offer stays waiting.
	let part = PartFile::create(dir)?;

	let listed = offers().await?.0.into_iter().find(|offer| offer.id == id);
	if let Some(offer) = &listed {
		check_name(&offer.name)?;
		space::check(dir, offer.size)?;
	}

	let mut transfer = client::open_transfer(Request::FileAccept { id: id.to_string() }).await?;
	let (name, size, from) = match &transfer.response {
		Response::FileIncoming {
			name, size, from, ..
		} => (name.clone(), *size, from.clone()),
		Response::Error { message } => anyhow::bail!("accept failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	};

	// The daemon already checked the name; this process is what writes, so it
	// checks for itself. And if the offer arrived after the listing above, its
	// space is checked now.
	let refusal = check_name(&name).err().or_else(|| match listed {
		Some(_) => None,
		None => space::check(dir, size).err(),
	});
	if let Some(refusal) = refusal {
		let _ = frame::write(&mut transfer.out, &Frame::Error(refusal.to_string())).await;
		return Err(refusal);
	}

	println!("receiving {name} ({}) from {from}...", format::size(size));

	let interrupted = super::interrupted();
	tokio::pin!(interrupted);
	tokio::select! {
		saved = receive(&mut transfer.frames, &mut transfer.out, part, dir, &name, size) => saved,
		// Dropping the transfer drops the partial file with it, and hanging up
		// tells the sender.
		_ = &mut interrupted => Err(Ended::error(Ended::CANCELLED, "cancelled; nothing was saved")),
	}
}

fn check_name(name: &str) -> Result<()> {
	filename::check(name, Platform::current())
		.map_err(|reason| anyhow::anyhow!("refusing the file {name:?}: {reason}"))
}

/// Writes the incoming frames to `part`, verifies them, and puts the file in
/// place under `name` — or leaves nothing behind.
///
/// `out` is where this side's verdict goes: an end frame once the file is in
/// place, which is what makes the transfer a delivery, or an error frame.
pub async fn receive<W: AsyncWrite + Unpin>(
	frames: &mut FrameReader,
	out: &mut W,
	mut part: PartFile,
	dir: &Path,
	name: &str,
	size: u64,
) -> Result<PathBuf> {
	let mut hasher = blake3::Hasher::new();
	let mut progress = format::Progress::new(size);
	let mut received: u64 = 0;
	let mut trailer = None;

	loop {
		match frames.next().await {
			Ok(Frame::Data(data)) => {
				received += data.len() as u64;
				if received > size {
					return Err(refuse(out, "more data arrived than was offered").await);
				}
				hasher.update(&data);
				if let Err(e) = part.write(&data).await {
					progress.done();
					return Err(
						refuse(out, &format!("could not write to {}: {e}", dir.display())).await,
					);
				}
				progress.update(received);
			}
			Ok(Frame::Hash(hash)) => trailer = Some(hash),
			Ok(Frame::End) => break,
			Ok(Frame::Error(message)) => {
				progress.done();
				anyhow::bail!("the transfer failed: {message}");
			}
			Ok(Frame::Control(_)) => {
				return Err(refuse(out, "unexpected message mid-transfer").await)
			}
			Err(_) => {
				progress.done();
				anyhow::bail!("the daemon went away mid-transfer; nothing was saved");
			}
		}
	}
	progress.done();

	// Nothing reaches its final name unless it is exactly what was offered and
	// exactly what the sender read.
	if received != size {
		return Err(refuse(out, &format!("received {received} bytes of {size}")).await);
	}
	if trailer != Some(*hasher.finalize().as_bytes()) {
		return Err(refuse(out, "the file did not match the sender's hash").await);
	}

	let saved = match part.finish(dir, name).await {
		Ok(saved) => saved,
		Err(e) => return Err(refuse(out, &e.to_string()).await),
	};

	// The commit. If it cannot be sent the file is still here and verified;
	// only the sender is left not knowing.
	if frame::write(out, &Frame::End).await.is_err() {
		eprintln!(
			"warning: saved, but the daemon could not be told, so the sender may report a failure"
		);
	}
	Ok(saved)
}

/// Tells the daemon this side is not keeping the file, and why, and returns
/// the reason as this command's error. Nothing was saved by then: the partial
/// file goes when its owner is dropped.
async fn refuse<W: AsyncWrite + Unpin>(out: &mut W, reason: &str) -> anyhow::Error {
	let _ = frame::write(out, &Frame::Error(reason.to_string())).await;
	anyhow::anyhow!("{reason}; nothing was saved")
}

/// The file being received, under a hidden temporary name in the destination
/// directory until it is complete and verified.
///
/// Deleted when dropped — by an error, a cancellation, Ctrl+C — so a transfer
/// that does not finish leaves nothing behind. (A process killed outright gets
/// no chance to clean up, and can leave a `.quix-*.part` file.)
pub struct PartFile {
	temp: tempfile::NamedTempFile,
	file: tokio::fs::File,
}

/// How far `photo (n).jpg` counts before giving up.
const MAX_SUFFIX: u32 = 9999;

impl PartFile {
	pub fn create(dir: &Path) -> Result<Self> {
		let mut builder = tempfile::Builder::new();
		builder.prefix(".quix-").suffix(".part");
		// Created like any other new file, subject to the user's umask, rather
		// than tempfile's owner-only default: it is about to be their file.
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			builder.permissions(std::fs::Permissions::from_mode(0o666));
		}
		let temp = builder
			.tempfile_in(dir)
			.map_err(|e| anyhow::anyhow!("cannot save into {}: {e}", dir.display()))?;
		let file = tokio::fs::File::from_std(temp.as_file().try_clone()?);
		Ok(Self { temp, file })
	}

	pub async fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
		self.file.write_all(data).await
	}

	/// Flushes the file to disk and moves it to `name`, or the first free
	/// `name (n)` — never over anything already there.
	pub async fn finish(mut self, dir: &Path, name: &str) -> Result<PathBuf> {
		self.file.flush().await?;
		self.file.sync_all().await?;
		drop(self.file);

		let mut temp = self.temp;
		for n in 0..=MAX_SUFFIX {
			let candidate = match n {
				0 => name.to_string(),
				n => filename::numbered(name, n),
			};
			let target = dir.join(&candidate);
			// Atomic and refusing to replace: renameat2(RENAME_NOREPLACE) or a
			// hard link on Unix, MoveFileEx without REPLACE_EXISTING on Windows.
			// Checking first and renaming after would race whoever else writes
			// here.
			match temp.persist_noclobber(&target) {
				Ok(_) => return Ok(target),
				Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => temp = e.file,
				Err(e) => anyhow::bail!("could not save {}: {}", target.display(), e.error),
			}
		}
		anyhow::bail!(
			"every name from {name} to {} is taken",
			filename::numbered(name, MAX_SUFFIX)
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn names_in(dir: &Path) -> Vec<String> {
		let mut names: Vec<String> = std::fs::read_dir(dir)
			.unwrap()
			.map(|e| e.unwrap().file_name().into_string().unwrap())
			.collect();
		names.sort();
		names
	}

	/// A daemon's side of a transfer, played by the test: `frames` are what it
	/// sends, and what comes back is the CLI's verdict.
	async fn run_receive(
		dir: &Path,
		name: &str,
		size: u64,
		frames: Vec<Frame>,
	) -> (Result<PathBuf>, Option<Frame>) {
		let (daemon, cli) = tokio::io::duplex(1 << 20);
		let (cli_rx, mut cli_tx) = tokio::io::split(cli);
		let (mut daemon_rx, mut daemon_tx) = tokio::io::split(daemon);
		for frame in &frames {
			frame::write(&mut daemon_tx, frame).await.unwrap();
		}
		// A real half-close: dropping one half of a split stream closes nothing.
		daemon_tx.shutdown().await.unwrap();

		let mut reader = FrameReader::spawn(cli_rx);
		let part = PartFile::create(dir).unwrap();
		let result = receive(&mut reader, &mut cli_tx, part, dir, name, size).await;
		// Both halves have to go before the daemon's side sees the end: the
		// reader's task holds the other one. Bounded, so a verdict that never
		// comes fails the test rather than hanging it.
		cli_tx.shutdown().await.unwrap();
		drop(reader);
		let verdict = tokio::time::timeout(
			std::time::Duration::from_secs(5),
			frame::read(&mut daemon_rx),
		)
		.await
		.expect("the CLI's side of the pipe closed")
		.ok();
		(result, verdict)
	}

	fn whole(data: &[u8]) -> Vec<Frame> {
		vec![
			Frame::Data(data.to_vec()),
			Frame::Hash(*blake3::hash(data).as_bytes()),
			Frame::End,
		]
	}

	#[tokio::test]
	async fn a_verified_file_lands_under_its_name_and_is_committed() {
		let dir = tempfile::tempdir().unwrap();
		let (saved, verdict) = run_receive(dir.path(), "photo.jpg", 5, whole(b"hello")).await;

		assert_eq!(saved.unwrap(), dir.path().join("photo.jpg"));
		assert_eq!(
			std::fs::read(dir.path().join("photo.jpg")).unwrap(),
			b"hello"
		);
		assert_eq!(verdict, Some(Frame::End), "committed only now");
		assert_eq!(
			names_in(dir.path()),
			["photo.jpg"],
			"no partial file left over"
		);
	}

	#[tokio::test]
	async fn an_existing_file_is_never_overwritten() {
		let dir = tempfile::tempdir().unwrap();
		std::fs::write(dir.path().join("photo.jpg"), b"original").unwrap();
		std::fs::write(dir.path().join("photo (1).jpg"), b"also original").unwrap();

		let (saved, _) = run_receive(dir.path(), "photo.jpg", 3, whole(b"new")).await;

		assert_eq!(saved.unwrap(), dir.path().join("photo (2).jpg"));
		assert_eq!(
			std::fs::read(dir.path().join("photo.jpg")).unwrap(),
			b"original"
		);
		assert_eq!(
			std::fs::read(dir.path().join("photo (1).jpg")).unwrap(),
			b"also original"
		);
		assert_eq!(
			std::fs::read(dir.path().join("photo (2).jpg")).unwrap(),
			b"new"
		);
	}

	#[tokio::test]
	async fn a_hash_mismatch_leaves_nothing_behind() {
		let dir = tempfile::tempdir().unwrap();
		let frames = vec![
			Frame::Data(b"hello".to_vec()),
			Frame::Hash([0; 32]),
			Frame::End,
		];

		let (saved, verdict) = run_receive(dir.path(), "photo.jpg", 5, frames).await;

		assert!(saved.unwrap_err().to_string().contains("hash"));
		assert!(
			matches!(verdict, Some(Frame::Error(_))),
			"the sender hears it failed"
		);
		assert!(
			names_in(dir.path()).is_empty(),
			"nothing under the final name, and no partial file"
		);
	}

	#[tokio::test]
	async fn a_missing_hash_leaves_nothing_behind() {
		let dir = tempfile::tempdir().unwrap();
		let frames = vec![Frame::Data(b"hello".to_vec()), Frame::End];
		let (saved, _) = run_receive(dir.path(), "a", 5, frames).await;
		assert!(saved.is_err());
		assert!(names_in(dir.path()).is_empty());
	}

	#[tokio::test]
	async fn a_short_file_leaves_nothing_behind() {
		let dir = tempfile::tempdir().unwrap();
		let (saved, verdict) = run_receive(dir.path(), "a", 10, whole(b"hello")).await;
		assert!(saved.unwrap_err().to_string().contains("5 bytes of 10"));
		assert!(matches!(verdict, Some(Frame::Error(_))));
		assert!(names_in(dir.path()).is_empty());
	}

	#[tokio::test]
	async fn more_data_than_offered_is_refused_at_once() {
		let dir = tempfile::tempdir().unwrap();
		let (saved, verdict) = run_receive(dir.path(), "a", 2, whole(b"hello")).await;
		assert!(saved.unwrap_err().to_string().contains("more data"));
		assert!(matches!(verdict, Some(Frame::Error(_))));
		assert!(names_in(dir.path()).is_empty());
	}

	#[tokio::test]
	async fn a_sender_that_vanishes_mid_transfer_leaves_nothing_behind() {
		let dir = tempfile::tempdir().unwrap();
		// Some data, then the connection just ends.
		let (saved, _) = run_receive(dir.path(), "a", 10, vec![Frame::Data(b"hel".to_vec())]).await;
		assert!(saved.unwrap_err().to_string().contains("nothing was saved"));
		assert!(
			names_in(dir.path()).is_empty(),
			"the .part file is gone too"
		);
	}

	#[tokio::test]
	async fn a_failure_reported_by_the_daemon_leaves_nothing_behind() {
		let dir = tempfile::tempdir().unwrap();
		let frames = vec![
			Frame::Data(b"hel".to_vec()),
			Frame::Error("nas went away mid-transfer".to_string()),
		];
		let (saved, _) = run_receive(dir.path(), "a", 10, frames).await;
		assert!(saved.unwrap_err().to_string().contains("went away"));
		assert!(names_in(dir.path()).is_empty());
	}

	#[test]
	fn a_part_file_is_hidden_and_goes_when_dropped() {
		let dir = tempfile::tempdir().unwrap();
		let runtime = tokio::runtime::Runtime::new().unwrap();
		let _guard = runtime.enter();

		let part = PartFile::create(dir.path()).unwrap();
		let names = names_in(dir.path());
		assert_eq!(names.len(), 1);
		assert!(
			names[0].starts_with(".quix-") && names[0].ends_with(".part"),
			"{names:?}"
		);

		drop(part);
		assert!(names_in(dir.path()).is_empty());
	}

	#[cfg(unix)]
	#[test]
	fn a_directory_the_caller_cannot_write_to_is_refused_with_the_os_error() {
		use std::os::unix::fs::PermissionsExt;
		if unsafe { libc::geteuid() } == 0 {
			eprintln!("skipped: root writes everywhere");
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
		let runtime = tokio::runtime::Runtime::new().unwrap();
		let _guard = runtime.enter();

		let refused = PartFile::create(dir.path()).err().unwrap().to_string();
		std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
		assert!(refused.contains("Permission denied"), "{refused}");
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn a_received_file_honours_the_umask_like_any_new_file() {
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		let (saved, _) = run_receive(dir.path(), "a", 5, whole(b"hello")).await;
		let mode = std::fs::metadata(saved.unwrap())
			.unwrap()
			.permissions()
			.mode() & 0o777;

		// The process umask, read without changing it for longer than a moment.
		let umask = unsafe {
			let old = libc::umask(0o022);
			libc::umask(old);
			old
		} as u32;
		assert_eq!(mode, 0o666 & !umask, "{mode:o}");
	}
}
