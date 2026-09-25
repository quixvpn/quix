//! `quix file`: send a file straight to another member, wormhole style.
//!
//! Nothing is uploaded anywhere first. `send` offers the file and waits; once
//! the other side accepts, the bytes go from this process, through both
//! daemons, into a file the receiving user's own process writes. Both ends have
//! to be there at the same time, and a transfer that breaks off is started
//! again rather than resumed.
//!
//! The files themselves are only ever opened here, in the CLI, as the user who
//! ran it — never by the daemon, which runs as root or SYSTEM. So a file this
//! user cannot read cannot be sent, a directory this user cannot write to
//! cannot receive, and whatever arrives belongs to this user.

mod dest;
mod format;
mod list;
mod receive;
mod send;
mod space;

use anyhow::Result;
use clap::{Args, Subcommand};

/// Send files to other members, and receive theirs
#[derive(Args)]
pub struct FileArgs {
	#[command(subcommand)]
	pub action: Action,
}

#[derive(Subcommand)]
pub enum Action {
	/// Offer a file to a peer and send it once they accept
	Send(send::SendArgs),
	/// Show offers waiting here, and pick one to accept or reject
	List(list::ListArgs),
	/// Accept an offer by id and save the file
	Accept(receive::AcceptArgs),
	/// Reject an offer by id
	Reject(receive::RejectArgs),
}

/// The window these arguments ask for, checked before anything else happens.
pub fn check_args(args: &FileArgs) -> Result<(), String> {
	match &args.action {
		Action::Send(args) => send::ttl_secs(args).map(|_| ()),
		_ => Ok(()),
	}
}

pub async fn run(args: FileArgs) -> Result<()> {
	match args.action {
		Action::Send(args) => send::run(args).await,
		Action::List(args) => list::run(args).await,
		Action::Accept(args) => receive::run_accept(args).await,
		Action::Reject(args) => receive::run_reject(args).await,
	}
}

/// How a transfer ended when that was not simply success or failure, carried
/// out of the command so `main` can exit with a code a script can branch on.
#[derive(Debug)]
pub struct Ended {
	pub code: i32,
	pub message: String,
}

impl Ended {
	/// The receiver said no.
	pub const REJECTED: i32 = 3;
	/// Nobody answered in time.
	pub const EXPIRED: i32 = 4;
	/// Ctrl+C, the conventional 128 + SIGINT.
	pub const CANCELLED: i32 = 130;

	pub fn error(code: i32, message: impl Into<String>) -> anyhow::Error {
		anyhow::Error::new(Self {
			code,
			message: message.into(),
		})
	}
}

impl std::fmt::Display for Ended {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.message)
	}
}

impl std::error::Error for Ended {}

/// Resolves when the user asks this process to stop: Ctrl+C, and on Unix the
/// terminal closing or a polite kill as well. A transfer races this, so that
/// stopping it runs the cleanup — the partial file removed — rather than
/// killing the process around it.
pub async fn interrupted() {
	#[cfg(unix)]
	{
		use tokio::signal::unix::{signal, SignalKind};
		let hangup = signal(SignalKind::hangup());
		let terminate = signal(SignalKind::terminate());
		if let (Ok(mut hangup), Ok(mut terminate)) = (hangup, terminate) {
			tokio::select! {
				_ = tokio::signal::ctrl_c() => {}
				_ = hangup.recv() => {}
				_ = terminate.recv() => {}
			}
			return;
		}
	}
	let _ = tokio::signal::ctrl_c().await;
}
