use anyhow::Result;
use clap::Args;
use proto::{PeerStatus, Request, Response, Traffic};

use super::client::send;

/// Show this node's addresses, its network, and the state of every peer link.
#[derive(Args)]
pub struct StatusArgs {
	/// Include per-hop packet counters and full endpoint ids
	#[arg(short, long)]
	pub verbose: bool,
}

pub async fn run(args: StatusArgs) -> Result<()> {
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
			println!("         {v4}");
			if args.verbose {
				println!("id       {endpoint_id}");
			}

			if peers.is_empty() {
				println!("\nno peers yet");
			} else {
				let linked = peers.iter().filter(|p| p.linked).count();
				println!("\npeers  {linked}/{} linked", peers.len());
				for peer in &peers {
					print_peer(peer, args.verbose);
				}
			}

			if args.verbose {
				print_traffic(&traffic);
			}
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("status failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}

fn print_peer(peer: &PeerStatus, verbose: bool) {
	let mark = if peer.linked { '●' } else { '○' };
	let id = if verbose {
		peer.id.clone()
	} else {
		short_id(&peer.id)
	};

	println!("  {mark} {}", peer.v6);
	println!("    {:<16} {id}", peer.v4);

	// Only worth showing when it's a problem: a link below the TUN MTU drops
	// full-size packets while small ones get through.
	match peer.datagram_max {
		Some(max) if max < MTU => println!("    datagram {max} — under the {MTU} MTU, large packets drop"),
		Some(max) if verbose => println!("    datagram {max}"),
		_ => {}
	}
}

fn print_traffic(traffic: &Traffic) {
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
}

/// The TUN MTU, mirrored from the daemon so status can flag a link below it.
const MTU: u32 = 1280;

/// Endpoint ids are 64 hex characters; the ends are enough to recognise one.
fn short_id(id: &str) -> String {
	match id.len() > 12 {
		true => format!("{}…{}", &id[..6], &id[id.len() - 4..]),
		false => id.to_string(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn short_id_keeps_both_ends() {
		let id = "dd0f06ddf61e34843f314341496c325238a0bef930431403d0aae6c332a448d2";
		assert_eq!(short_id(id), "dd0f06…48d2");
	}

	#[test]
	fn short_id_leaves_already_short_ids_alone() {
		assert_eq!(short_id("abc123"), "abc123");
	}
}
