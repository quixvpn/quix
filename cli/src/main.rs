use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use proto::{Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(name = "quix")]
struct Cli {
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
enum Command {
	Ping { peer: String },
}

#[tokio::main]
async fn main() -> Result<()> {
	let cli = Cli::parse();

	match cli.command {
		Command::Ping { peer } => {
			let req = Request::Ping {
				peer,
				msg: "hello from quix".to_string(),
			};
			let resp = send(req).await?;
			match resp {
				Response::Ok { echo } => println!("echo: {echo}"),
				Response::Error { message } => anyhow::bail!("ping failed: {message}"),
			}
		}
	}

	Ok(())
}

async fn send(req: Request) -> Result<Response> {
	let mut stream = UnixStream::connect(proto::socket_path())
		.await
		.context("is quixd running?")?;

	let payload = serde_json::to_vec(&req)?;
	stream.write_all(&payload).await?;
	stream.shutdown().await?; // signal "done writing" so the daemon's read_to_end unblocks

	let mut buf = Vec::new();
	stream.read_to_end(&mut buf).await?;

	let resp: Response = serde_json::from_slice(&buf)?;
	Ok(resp)
}