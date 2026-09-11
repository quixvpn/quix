use anyhow::{Context, Result};
use proto::{Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub async fn send(req: Request) -> Result<Response> {
	let mut stream = UnixStream::connect(proto::socket_path())
		.await
		.context("is quixd running?")?;

	let payload = serde_json::to_vec(&req)?;
	stream.write_all(&payload).await?;
	stream.shutdown().await?;

	let mut buf = Vec::new();
	stream.read_to_end(&mut buf).await?;

	Ok(serde_json::from_slice(&buf)?)
}