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
			zone,
		} => {
			match network_name {
				Some(name) => println!("joined network: {name}"),
				None => println!("joined network"),
			}
			if let Some(hostname) = hostname {
				println!("hostname: {hostname}{}", reachable_at(&hostname, &zone));
			}
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("join failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}

/// The parenthesised name to show after a hostname, or nothing when the daemon
/// did not say what zone it is in.
///
/// The name given is the qualified one — `nas.homelab.quix` — which is what
/// `status`, the roster and every other peer call this machine. The flat
/// `nas.quix` this used to print does resolve, so nobody was stuck, but it is
/// not the name anyone else sees, and two spellings of the same machine is one
/// more than a first-run message should introduce.
fn reachable_at(hostname: &str, zone: &str) -> String {
	// A daemon that predates the field tells us nothing, and a name built from a
	// zone we were not given would be a guess. The hostname alone is still true.
	match zone.is_empty() {
		true => String::new(),
		false => format!(" (reachable at {hostname}.{zone})"),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_name_is_shown_qualified_by_its_network() {
		// Not `nas.quix`: that resolves, but it is not what the peer is called
		// anywhere else.
		assert_eq!(
			reachable_at("nas", "homelab.quix"),
			" (reachable at nas.homelab.quix)"
		);
	}

	#[test]
	fn a_daemon_that_did_not_say_gets_no_invented_name() {
		assert_eq!(reachable_at("nas", ""), "");
	}
}