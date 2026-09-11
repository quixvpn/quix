use anyhow::Result;
use clap::Args;
use proto::{Request, Response};

use super::client::send;

/// Show the daemon's endpoint id and connected peer count.
#[derive(Args)]
pub struct StatusArgs;

pub async fn run(_args: StatusArgs) -> Result<()> {
	match send(Request::Status).await? {
		Response::Status {
			endpoint_id,
			peer_count,
		} => {
			println!("Endpoint ID: {endpoint_id}");
			println!("Connected Peers: {peer_count}");
			Ok(())
		}
		Response::Ok { .. } => anyhow::bail!("unexpected ok response to status"),
		Response::Error { message } => anyhow::bail!("status failed: {message}"),
	}
}