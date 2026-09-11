use anyhow::Result;
use clap::Args;
use proto::{Request, Response};

use super::client::send;

/// Send a test message to a peer
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
		Response::Error { message } => anyhow::bail!("ping failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}