use anyhow::{Context, Result};
use interprocess::local_socket::tokio::{prelude::*, Stream};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ToFsName, ToNsName};
use proto::{Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub async fn send(req: Request) -> Result<Response> {
	let raw = proto::socket_name();

	let name = if cfg!(windows) {
		raw.to_ns_name::<GenericNamespaced>()?
	} else {
		raw.to_fs_name::<GenericFilePath>()?
	};

	let mut stream = Stream::connect(name)
		.await
		.context("is quixd running?")?;

	let payload = serde_json::to_vec(&req)?;
	stream.write_all(&payload).await?;
	stream.shutdown().await?;

	let mut buf = Vec::new();
	stream.read_to_end(&mut buf).await?;

	Ok(serde_json::from_slice(&buf)?)
}