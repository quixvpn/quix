use anyhow::Result;
use clap::Args;
use proto::{Request, Response};

use super::client::send;

/// Remove a peer from the network (coordinator only)
#[derive(Args)]
pub struct KickArgs {
	/// The peer as `quix status` shows it: hostname, fallback id, either with
	/// the zone, or the full endpoint id
	pub peer: String,
}

pub async fn run(args: KickArgs) -> Result<()> {
	match send(Request::Kick { peer: args.peer }).await? {
		Response::Kicked { name, id, notified } => {
			println!("kicked {name} ({id})");
			if !notified {
				println!(
					"NOTE: {name} could not be reached, so it still thinks it is in the network. \
					 Members refuse its links once they have the new roster."
				);
			}
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("kick failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}
