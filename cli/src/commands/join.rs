use anyhow::Result;
use clap::Args;
use proto::{Request, Response};

use super::client::send;

/// Join a network using an invite code
#[derive(Args)]
pub struct JoinArgs {
	pub code: String,
}

pub async fn run(args: JoinArgs) -> Result<()> {
	match send(Request::Join { code: args.code }).await? {
		Response::Joined { network_name } => {
			match network_name {
				Some(name) => println!("joined network: {name}"),
				None => println!("joined network"),
			}
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("join failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}