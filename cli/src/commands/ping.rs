use anyhow::{Context, Result};
use clap::Args;
use proto::{Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Args)]
pub struct PingArgs {
    /// The endpoint id of the peer to ping
	pub peer: String,
}

pub async fn run(args: PingArgs) -> Result<()> {
	let req = Request::Ping {
		peer: args.peer,
		msg: "hello from quix".to_string(),
	};

	match send(req).await? {
	Response::Ok { echo } => {
		println!("echo: {echo}");
		Ok(())
	}
	Response::Status { .. } => {
		anyhow::bail!("unexpected status response to ping")
	}
	Response::Error { message } => anyhow::bail!("ping failed: {message}"),
}
}

async fn send(req: Request) -> Result<Response> {
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