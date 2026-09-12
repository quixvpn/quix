use anyhow::{Context, Result};
use interprocess::local_socket::tokio::{prelude::*, Stream};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ToFsName, ToNsName};
use proto::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub async fn send(req: Request) -> Result<Response> {
	let raw = proto::socket_name();

	let name = if cfg!(windows) {
		raw.to_ns_name::<GenericNamespaced>()?
	} else {
		raw.to_fs_name::<GenericFilePath>()?
	};

	let conn = Stream::connect(name)
		.await
		.context("is quixd running?")?;

	let mut recver = BufReader::new(&conn);
	let mut sender = &conn;

	let mut payload = serde_json::to_vec(&req)?;
	payload.push(b'\n');
	sender.write_all(&payload).await?;

	let mut line = String::new();
	recver.read_line(&mut line).await?;

	let response: Response = serde_json::from_str(line.trim())?;

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