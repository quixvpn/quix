use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use proto::filename::{self, Platform};
use proto::frame::{self, Frame, CHUNK};
use proto::{FileEvent, Request, Response, DEFAULT_FILE_TTL, MAX_FILE_TTL};
use tokio::io::AsyncReadExt;

use super::format;
use super::Ended;
use crate::commands::client::{self, Transfer};

/// Offer a file to a peer and send it once they accept
#[derive(Args)]
pub struct SendArgs {
	/// The file to send. Only regular files: directories are refused
	pub path: PathBuf,
	/// The peer: its hostname, `name.network.quix`, its 8-character fallback id,
	/// or its overlay IP address
	pub host: String,
	/// How long the offer waits for an answer, e.g. `--expires 30 min`
	///
	/// Units: min, hours, days. Defaults to 10 minutes; at most 24 hours. This
	/// command stays running until the offer is answered or expires.
	#[arg(long, num_args = 2, value_names = ["AMOUNT", "UNIT"])]
	pub expires: Option<Vec<String>>,
}

pub fn ttl_secs(args: &SendArgs) -> Result<u64, String> {
	match &args.expires {
		Some(parts) => {
			crate::commands::expiry::parse(&parts[0], &parts[1], MAX_FILE_TTL, "a file offer")
		}
		None => Ok(DEFAULT_FILE_TTL),
	}
}

/// A file opened and checked, ready to offer.
pub struct Prepared {
	pub file: std::fs::File,
	pub name: String,
	pub size: u64,
}

/// Everything about the file that can be checked without the daemon, checked
/// before the daemon is contacted.
///
/// The file is opened here, by this process, with the permissions of whoever
/// ran it. Nothing else ever opens it: the daemon is never told a path. So a
/// file this user cannot read fails here with the operating system's own
/// answer, and there is no request that could make the daemon read it instead.
pub fn prepare(path: &Path) -> Result<Prepared> {
	let shown = path.display();
	let metadata =
		std::fs::metadata(path).map_err(|e| anyhow::anyhow!("cannot send {shown}: {e}"))?;
	if metadata.is_dir() {
		anyhow::bail!("cannot send {shown}: it is a directory, and only regular files can be sent");
	}

	let file =
		std::fs::File::open(path).map_err(|e| anyhow::anyhow!("cannot send {shown}: {e}"))?;
	// Judged again on what was actually opened, which is what will be read:
	// the path may have been swapped for something else in between.
	let metadata = file
		.metadata()
		.map_err(|e| anyhow::anyhow!("cannot send {shown}: {e}"))?;
	if !metadata.is_file() {
		anyhow::bail!("cannot send {shown}: it is not a regular file");
	}

	// The last component only: where the file lives here is nobody else's
	// business, and the receiver refuses any name with a directory in it.
	let name = path
		.file_name()
		.ok_or_else(|| anyhow::anyhow!("cannot send {shown}: it has no file name"))?
		.to_str()
		.ok_or_else(|| anyhow::anyhow!("cannot send {shown}: its name is not valid Unicode"))?
		.to_string();
	filename::check(&name, Platform::current())
		.map_err(|reason| anyhow::anyhow!("cannot send {shown}: {reason}"))?;

	Ok(Prepared {
		file,
		name,
		size: metadata.len(),
	})
}

pub async fn run(args: SendArgs) -> Result<()> {
	let ttl_secs = ttl_secs(&args).map_err(|e| anyhow::anyhow!(e))?;
	let prepared = prepare(&args.path)?;
	let (name, size) = (prepared.name.clone(), prepared.size);

	let mut transfer = client::open_transfer(Request::FileSend {
		name: name.clone(),
		size,
		target: args.host.clone(),
		ttl_secs,
	})
	.await?;

	let (to, expires_in_secs) = match &transfer.response {
		Response::FileOffered {
			to,
			expires_in_secs,
			..
		} => (to.clone(), *expires_in_secs),
		Response::Error { message } => anyhow::bail!("send failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	};

	// Installed once for the whole command: from here, Ctrl+C withdraws the
	// offer or abandons the transfer rather than killing the process mid-way.
	let interrupted = super::interrupted();
	tokio::pin!(interrupted);

	// At a terminal the time left counts down on a line of its own below this
	// one, short enough never to wrap, since a wrapped line cannot be redrawn
	// in place. Anywhere else it is said once, here.
	let mut countdown = format::Countdown::new(expires_in_secs);
	let expiry = match countdown.enabled() {
		true => String::new(),
		false => format!(", expires in {}", format::duration(expires_in_secs)),
	};
	println!(
		"Waiting for {to} to accept {name} ({}){expiry}... press Ctrl+C to cancel",
		format::size(size),
	);

	let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
	tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	let answer = loop {
		tokio::select! {
			answer = transfer.frames.next() => break Some(answer),
			_ = &mut interrupted => break None,
			_ = tick.tick(), if countdown.enabled() => countdown.show(),
		}
	};
	countdown.done();
	let Some(answer) = answer else {
		return Err(Ended::error(
			Ended::CANCELLED,
			"cancelled; the offer was withdrawn",
		));
	};
	match answer {
		Ok(Frame::Control(json)) => match Frame::parse::<FileEvent>(&json)? {
			FileEvent::Accepted => {}
			FileEvent::Rejected => {
				return Err(Ended::error(
					Ended::REJECTED,
					format!("{to} rejected {name}"),
				))
			}
			FileEvent::Expired => {
				return Err(Ended::error(
					Ended::EXPIRED,
					format!("{to} did not answer in time; the offer expired"),
				))
			}
			FileEvent::Delivered => {
				anyhow::bail!("the daemon reported a delivery before any transfer")
			}
		},
		Ok(Frame::Error(message)) => anyhow::bail!("send failed: {message}"),
		Ok(_) => anyhow::bail!("unexpected message from the daemon"),
		Err(_) => anyhow::bail!("the daemon went away"),
	}

	println!("{to} accepted, sending...");
	tokio::select! {
		sent = stream(prepared, &mut transfer) => sent?,
		_ = &mut interrupted => return Err(Ended::error(Ended::CANCELLED, "cancelled; the transfer was abandoned")),
	}

	// Sent is not delivered: the receiver still has to verify it and put it
	// in place, which for a large file includes flushing it to disk.
	let outcome = tokio::select! {
		outcome = transfer.frames.next() => outcome,
		_ = &mut interrupted => return Err(Ended::error(Ended::CANCELLED, "stopped waiting; whether the file arrived is unknown")),
	};
	match outcome {
		Ok(Frame::Control(json)) if Frame::parse::<FileEvent>(&json)? == FileEvent::Delivered => {
			println!("Delivered {name} to {to}");
			Ok(())
		}
		Ok(Frame::Error(message)) => anyhow::bail!("send failed: {message}"),
		Ok(_) => anyhow::bail!("unexpected message from the daemon"),
		Err(_) => anyhow::bail!("the daemon went away before {to} confirmed the file arrived"),
	}
}

/// Reads the file once, hashing as it goes, and sends it as data frames
/// followed by the hash and an end frame.
async fn stream(prepared: Prepared, transfer: &mut Transfer) -> Result<()> {
	let Prepared { file, name, size } = prepared;
	let mut file = tokio::fs::File::from_std(file);
	let mut hasher = blake3::Hasher::new();
	let mut progress = format::Progress::new(size);
	let mut buf = vec![0u8; CHUNK];
	let mut sent: u64 = 0;

	loop {
		let n = match file.read(&mut buf).await {
			Ok(n) => n,
			Err(e) => {
				let _ = frame::write(&mut transfer.out, &Frame::Error(e.to_string())).await;
				anyhow::bail!("reading {name} failed: {e}");
			}
		};
		if n == 0 {
			break;
		}
		sent += n as u64;
		if sent > size {
			let reason = "the file grew while it was being sent";
			let _ = frame::write(&mut transfer.out, &Frame::Error(reason.to_string())).await;
			anyhow::bail!("{reason}");
		}
		hasher.update(&buf[..n]);

		// The daemon only speaks mid-transfer to say it is over, so a failed
		// write is followed by the reason.
		if frame::write(&mut transfer.out, &Frame::Data(buf[..n].to_vec()))
			.await
			.is_err()
		{
			return Err(broken_off(transfer).await);
		}
		progress.update(sent);
	}
	progress.done();

	if sent != size {
		let reason = "the file shrank while it was being sent";
		let _ = frame::write(&mut transfer.out, &Frame::Error(reason.to_string())).await;
		anyhow::bail!("{reason}");
	}

	let hash = *hasher.finalize().as_bytes();
	if frame::write(&mut transfer.out, &Frame::Hash(hash))
		.await
		.is_err()
		|| frame::write(&mut transfer.out, &Frame::End).await.is_err()
	{
		return Err(broken_off(transfer).await);
	}
	Ok(())
}

/// Why the daemon stopped taking frames.
async fn broken_off(transfer: &mut Transfer) -> anyhow::Error {
	match transfer.frames.next().await {
		Ok(Frame::Error(message)) => anyhow::anyhow!("send failed: {message}"),
		_ => anyhow::anyhow!("send failed: the daemon stopped the transfer"),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_regular_file_is_prepared_with_its_bare_name_and_size() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("report.pdf");
		std::fs::write(&path, b"12345").unwrap();

		let prepared = prepare(&path).unwrap();
		assert_eq!(prepared.name, "report.pdf", "never the directory it is in");
		assert_eq!(prepared.size, 5);
	}

	#[test]
	fn a_directory_is_refused_by_name() {
		let dir = tempfile::tempdir().unwrap();
		let refused = prepare(dir.path()).err().unwrap().to_string();
		assert!(refused.contains("directory"), "{refused}");
	}

	#[test]
	fn a_missing_file_is_the_operating_systems_error() {
		let dir = tempfile::tempdir().unwrap();
		let refused = prepare(&dir.path().join("nope")).err().unwrap().to_string();
		assert!(refused.contains("nope"), "{refused}");
	}

	#[cfg(unix)]
	#[test]
	fn a_file_the_caller_cannot_read_is_refused_with_the_os_error() {
		use std::os::unix::fs::PermissionsExt;
		if unsafe { libc::geteuid() } == 0 {
			eprintln!("skipped: root reads everything");
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("secret");
		std::fs::write(&path, b"x").unwrap();
		std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

		let refused = prepare(&path).err().unwrap().to_string();
		assert!(refused.contains("Permission denied"), "{refused}");
	}

	#[cfg(unix)]
	#[test]
	fn something_that_is_not_a_regular_file_is_refused() {
		let refused = prepare(Path::new("/dev/null")).err().unwrap().to_string();
		assert!(refused.contains("not a regular file"), "{refused}");
	}

	#[test]
	fn the_offer_window_defaults_to_ten_minutes_and_stops_at_a_day() {
		let args = |expires: Option<[&str; 2]>| SendArgs {
			path: PathBuf::from("a"),
			host: "nas".to_string(),
			expires: expires.map(|e| e.iter().map(|s| s.to_string()).collect()),
		};
		assert_eq!(ttl_secs(&args(None)), Ok(600));
		assert_eq!(ttl_secs(&args(Some(["30", "min"]))), Ok(1800));
		assert_eq!(ttl_secs(&args(Some(["24", "hours"]))), Ok(MAX_FILE_TTL));
		assert!(
			ttl_secs(&args(Some(["2", "days"]))).is_err(),
			"past the cap"
		);
		assert!(ttl_secs(&args(Some(["5", "fortnights"]))).is_err());
	}
}
