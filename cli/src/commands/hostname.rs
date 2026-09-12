use anyhow::Result;
use clap::Args;
use proto::{Request, Response};

use super::client::send;

/// Set this machine's hostname on the mesh
#[derive(Args)]
pub struct HostnameArgs {
	/// Lowercase letters, digits and hyphens, up to 63 characters
	pub name: String,
	/// Take a name currently bound to another key.
	///
	/// For a machine rebuilt under a new identity reclaiming the name it used
	/// to hold. Every other member reports the change rather than applying it
	/// silently, which is the difference between administration and a
	/// redirect.
	#[arg(long)]
	pub force: bool,
}

pub async fn run(args: HostnameArgs) -> Result<()> {
	let requested = args.name.clone();
	match send(Request::SetHostname {
		hostname: args.name,
		force: args.force,
	})
	.await?
	{
		Response::HostnameSet { hostname, .. } => {
			println!("hostname set to {hostname}");
			if hostname != requested.to_lowercase() {
				println!("({requested} was taken, so a suffix was added)");
			}
			println!("reachable at {hostname}.quix");
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("hostname failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}
