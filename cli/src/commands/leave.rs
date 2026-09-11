use anyhow::Result;
use clap::Args;
use proto::{Request, Response};

use super::client::send;

/// Leave the current network, dropping every peer link
#[derive(Args)]
pub struct LeaveArgs;

pub async fn run(_args: LeaveArgs) -> Result<()> {
	match send(Request::Leave).await? {
		Response::Left {
			network_name,
			coordinator_notified,
		} => {
			match network_name {
				Some(name) => println!("Sucessfully left {name}."),
				None => println!("Not a member of any network!"),
			}
			if !coordinator_notified {
				println!("NOTE: The coordinator could not be reached, so it may still list you.");
			}
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("Leave failed: {message}"),
		_ => anyhow::bail!("Unexpected response."),
	}
}
