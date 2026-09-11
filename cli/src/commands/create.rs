use anyhow::Result;
use clap::Args;
use proto::{Request, Response};

use super::client::send;

/// Create a new network and become its coordinator
#[derive(Args)]
pub struct CreateArgs {
	pub name: String,
}

pub async fn run(args: CreateArgs) -> Result<()> {
	match send(Request::CreateNetwork { name: args.name }).await? {
		Response::Ok { .. } => {
			println!("network created, you are the coordinator");
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("create failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}