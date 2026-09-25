use anyhow::{Context, Result};
use interprocess::local_socket::tokio::{prelude::*, RecvHalf, SendHalf, Stream};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ToFsName, ToNsName};
use proto::frame::FrameReader;
use proto::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn connect() -> Result<Stream> {
	let raw = proto::socket_name();

	let name = if cfg!(windows) {
		raw.to_ns_name::<GenericNamespaced>()?
	} else {
		raw.to_fs_name::<GenericFilePath>()?
	};

	Stream::connect(name).await.context("is quixd running?")
}

/// One request, one response line, with no elevation and no interpretation.
async fn ask(req: &Request) -> Result<Response> {
	let conn = connect().await?;

	let mut recver = BufReader::new(&conn);
	let mut sender = &conn;

	let mut payload = serde_json::to_vec(req)?;
	payload.push(b'\n');
	sender.write_all(&payload).await?;

	let mut line = String::new();
	recver.read_line(&mut line).await?;

	Ok(serde_json::from_str(line.trim())?)
}

pub async fn send(req: Request) -> Result<Response> {
	let response = ask(&req).await?;

	// Ask first, prompt only if the answer was no.
	//
	// The operator — whoever installed quix — is authorized already, so the
	// common case never sees a UAC dialog. Anyone else gets one exactly when it
	// would change the outcome, rather than on every mutating command. Nothing
	// has happened at this point: authorization is checked before the daemon
	// acts, so re-running the command elevated repeats no work.
	#[cfg(windows)]
	if matches!(response, Response::Unauthorized { .. }) && !crate::elevate::is_elevated() {
		crate::elevate::relaunch();
	}

	// Raised here rather than left to each command, which would otherwise meet
	// it in a catch-all arm and report "unexpected response" over a message
	// that already says who was refused and how to fix it.
	if let Response::Unauthorized { message } = response {
		anyhow::bail!("{message}");
	}

	Ok(response)
}

/// Like [`send`], but never retried elevated.
///
/// For file commands, which must run as the user who typed them: the files they
/// read and write are that user's, with that user's permissions. An elevated
/// relaunch would create files owned by Administrators rather than the user,
/// and would take an interactive list away from the terminal it was run in.
pub async fn send_as_caller(req: Request) -> Result<Response> {
	match ask(&req).await? {
		Response::Unauthorized { message } => anyhow::bail!("{message}{}", ELEVATION_NOTE),
		response => Ok(response),
	}
}

#[cfg(windows)]
const ELEVATION_NOTE: &str = "\n(file transfers are never retried elevated: the file would then \
	belong to Administrators instead of you)";
#[cfg(not(windows))]
const ELEVATION_NOTE: &str = "";

/// A file transfer in progress: the daemon's first answer, then frames both
/// ways for as long as the transfer lasts.
pub struct Transfer {
	pub response: Response,
	/// Everything the daemon says after its first answer.
	pub frames: FrameReader,
	/// Where the CLI's own frames go.
	pub out: SendHalf,
}

/// Starts a transfer. Never elevated, for the reasons on [`send_as_caller`].
pub async fn open_transfer(req: Request) -> Result<Transfer> {
	let (recv, mut out) = connect().await?.split();

	let mut payload = serde_json::to_vec(&req)?;
	payload.push(b'\n');
	out.write_all(&payload).await?;

	// The frames that follow the answer are read through the same buffer, so
	// none that arrive with the answer line are lost.
	let mut recv: BufReader<RecvHalf> = BufReader::new(recv);
	let mut line = String::new();
	recv.read_line(&mut line).await?;
	if line.is_empty() {
		anyhow::bail!("the daemon closed the connection without answering");
	}

	let response = match serde_json::from_str(line.trim())? {
		Response::Unauthorized { message } => anyhow::bail!("{message}{}", ELEVATION_NOTE),
		response => response,
	};

	Ok(Transfer {
		response,
		frames: FrameReader::spawn(recv),
		out,
	})
}
