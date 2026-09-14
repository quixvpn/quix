use anyhow::Result;
use clap::Args;
use proto::{DnsRegistration, PeerStatus, Request, Response, Traffic};

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
			name,
			named,
			v6,
			v4,
			network,
			coordinator,
			peers,
			zone,
			traffic,
			conflicts,
			dns,
		} => {
			let role = if coordinator { "coordinator" } else { "member" };
			match network {
				Some(name) => println!("network  {name}  ({role})"),
				None => println!("network  none — run `quix create <name>` or `quix join <code>`"),
			}
			let unnamed = if named { "" } else { "  (no hostname set)" };
			println!("name ----  {name}.{zone}{unnamed}");
			println!("IPv4 ----  {v4}");
			println!("IPv6 ----  {v6}");
			println!("id ------  {endpoint_id}");
			// Never behind -v: broken name resolution with nothing on screen to
			// explain it is exactly the failure this line exists for.
			if let Some(line) = dns_line(&dns) {
				println!("{line}");
			}

			if peers.is_empty() {
				println!("\nno peers yet");
			} else {
				let linked = peers.iter().filter(|p| p.linked).count();
				println!("\npeers  {linked}/{} linked", peers.len());
				for peer in &peers {
					print_peer(peer, &zone, args.verbose);
				}
			}

			// A refused rebinding is the only signal that something tried to
			// point an existing name at a different key, so it is never hidden
			// behind -v.
			if !conflicts.is_empty() {
				println!("\nname conflicts");
				for conflict in &conflicts {
					println!("  {conflict}");
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

fn print_peer(peer: &PeerStatus, zone: &str, verbose: bool) {
	let mark = if peer.linked { '●' } else { '○' };
	// Name and full id side by side: the name is what you type, the id is what
	// actually identifies the peer and is worth being able to check.
	let unnamed = if peer.named { "" } else { "  (no hostname set)" };

	println!("  {mark} {}.{zone}{unnamed}", peer.name);
	println!("    {:<16} {}", peer.v4, peer.id);
	println!("    {}", peer.v6);

	// Only worth showing when it's a problem: a link below the TUN MTU drops
	// full-size packets while small ones get through.
	match peer.datagram_max {
		Some(max) if max < MTU => println!("    datagram {max} — under the {MTU} MTU, large packets drop"),
		Some(max) if verbose => println!("    datagram {max}"),
		_ => {}
	}
}

/// Says why names do not resolve through the system, or nothing when they do.
fn dns_line(dns: &DnsRegistration) -> Option<String> {
	match dns {
		DnsRegistration::Registered => None,
		DnsRegistration::Retrying { fallback } => Some(format!(
			"dns -----  not registered with the system resolver yet, retrying — \
			 names resolve only via {fallback} meanwhile"
		)),
		DnsRegistration::Unavailable => Some(
			"dns -----  not registered with the system resolver — the daemon log says why".to_string(),
		),
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

#[cfg(test)]
mod tests {
	use super::*;
	use proto::DnsRegistration;

	#[test]
	fn nothing_is_said_when_names_resolve_normally() {
		assert_eq!(dns_line(&DnsRegistration::Registered), None);
	}

	#[test]
	fn a_registration_being_retried_says_so_and_where_names_resolve_meanwhile() {
		let line = dns_line(&DnsRegistration::Retrying {
			fallback: "127.0.0.1:5354".to_string(),
		})
		.expect("a broken resolver must be visible");

		assert!(line.contains("retrying"), "{line}");
		assert!(line.contains("127.0.0.1:5354"), "{line}");
	}

	#[test]
	fn a_registration_nobody_is_retrying_does_not_claim_to_be() {
		let line = dns_line(&DnsRegistration::Unavailable).expect("a broken resolver must be visible");

		assert!(!line.contains("retrying"), "{line}");
		assert!(line.contains("log"), "points somewhere to find out why: {line}");
	}

	#[test]
	fn a_daemon_from_before_the_field_reads_as_registered() {
		// A newer CLI talking to an older daemon must not warn about a problem the
		// daemon never reported.
		let status = Response::Status {
			endpoint_id: String::new(),
			name: String::new(),
			named: false,
			v6: String::new(),
			v4: String::new(),
			network: None,
			coordinator: false,
			peers: vec![],
			zone: "quix".to_string(),
			traffic: Traffic::default(),
			conflicts: vec![],
			dns: DnsRegistration::Unavailable,
		};
		let mut json = serde_json::to_value(&status).unwrap();
		json["Status"].as_object_mut().unwrap().remove("dns");

		match serde_json::from_value::<Response>(json).unwrap() {
			Response::Status { dns, .. } => assert_eq!(dns, DnsRegistration::Registered),
			other => panic!("expected a status, got {other:?}"),
		}
	}
}
