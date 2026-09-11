use anyhow::Result;
use clap::Args;
use proto::{Request, Response};

use super::client::send;

/// Probe a peer's mesh link, bringing one up if it isn't already.
#[derive(Args)]
pub struct PingArgs {
	/// The endpoint id of the peer to probe
	pub peer: String,
}

pub async fn run(args: PingArgs) -> Result<()> {
	match send(Request::Ping { peer: args.peer }).await? {
		Response::Pong { virtual_ip, rtt_ms } => {
			match rtt_ms {
				Some(rtt) => println!("{virtual_ip}  linked  rtt {rtt:.1}ms"),
				None => println!("{virtual_ip}  linked"),
			}
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("ping failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}
