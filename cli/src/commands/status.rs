use anyhow::Result;
use clap::Args;
use proto::{Request, Response};

use super::client::send;

/// Show this node's addresses, its network, and the state of every peer link.
#[derive(Args)]
pub struct StatusArgs;

pub async fn run(_args: StatusArgs) -> Result<()> {
	match send(Request::Status).await? {
		Response::Status {
			endpoint_id,
			v6,
			v4,
			network,
			coordinator,
			peers,
			traffic,
		} => {
			let role = if coordinator { "coordinator" } else { "member" };
			match network {
				Some(name) => println!("network  {name}  ({role})"),
				None => println!("network  none — run `quix create <name>` or `quix join <code>`"),
			}
			println!("address  {v6}");
			println!("         {v4}  (IPv4 compatibility)");
			println!("id       {endpoint_id}");

			// Ordered as a packet travels, so the first zero is the failing hop.
			println!(
				"\noutbound  tun read {}  ->  sent {}",
				traffic.tun_rx, traffic.mesh_tx
			);
			println!(
				"inbound   received {}  ->  tun write {}",
				traffic.mesh_rx, traffic.tun_tx
			);

			let drops = [
				("not a member", traffic.no_route),
				("no link", traffic.no_link),
				("too big", traffic.oversize),
				("send failed", traffic.send_err),
				("tun write failed", traffic.tun_tx_err),
			];
			let dropped: Vec<String> = drops
				.iter()
				.filter(|(_, n)| *n > 0)
				.map(|(label, n)| format!("{label} {n}"))
				.collect();
			if !dropped.is_empty() {
				println!("dropped   {}", dropped.join("  "));
			}

			if peers.is_empty() {
				println!("\nno peers yet");
				return Ok(());
			}

			let linked = peers.iter().filter(|p| p.linked).count();
			println!("\npeers  {linked}/{} linked", peers.len());
			for peer in peers {
				let mark = if peer.linked { '●' } else { '○' };
				println!("  {mark} {}", peer.v6);
				println!("    {:<16} {}", peer.v4, peer.id);
			}
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("status failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}
