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
		Response::Pong { v6, v4, rtt_ms } => {
			match rtt_ms {
				Some(rtt) => println!("{v6}  linked  rtt {rtt:.1}ms"),
				None => println!("{v6}  linked"),
			}
			println!("{v4}  (IPv4 compatibility)");
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("ping failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}
