use anyhow::Result;
use clap::Args;
use proto::{Request, Response};

use super::client::send;

/// Join a network using an invite code
#[derive(Args)]
pub struct JoinArgs {
	pub code: String,
	/// This machine's hostname on the network being joined
	#[arg(long)]
	pub hostname: Option<String>,
}

pub async fn run(args: JoinArgs) -> Result<()> {
	match send(Request::Join {
		code: args.code,
		hostname: args.hostname,
	}).await? {
		Response::Joined {
			network_name,
			hostname,
		} => {
			match network_name {
				Some(name) => println!("joined network: {name}"),
				None => println!("joined network"),
			}
			if let Some(hostname) = hostname {
				println!("Hostname: {hostname} (reachable at {hostname}.quix)");
			}
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("join failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}