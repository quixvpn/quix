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

	Ok(serde_json::from_str(line.trim())?)
}