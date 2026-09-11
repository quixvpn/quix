use anyhow::Result;
use clap::Args;
use proto::{Request, Response};

use super::client::send;

/// Show this node's address, its network, and the state of every peer link.
#[derive(Args)]
pub struct StatusArgs;

pub async fn run(_args: StatusArgs) -> Result<()> {
	match send(Request::Status).await? {
		Response::Status {
			endpoint_id,
			virtual_ip,
			network,
			coordinator,
			peers,
		} => {
			let role = if coordinator { "coordinator" } else { "member" };
			match network {
				Some(name) => println!("network  {name}  ({role})"),
				None => println!("network  none — run `quix create <name>` or `quix join <code>`"),
			}
			println!("address  {virtual_ip}");
			println!("id       {endpoint_id}");

			if peers.is_empty() {
				println!("\nno peers yet");
				return Ok(());
			}

			let linked = peers.iter().filter(|p| p.linked).count();
			println!("\npeers  {linked}/{} linked", peers.len());
			for peer in peers {
				let mark = if peer.linked { '●' } else { '○' };
				println!("  {mark} {:<15}  {}", peer.virtual_ip, peer.id);
			}
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("status failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}
