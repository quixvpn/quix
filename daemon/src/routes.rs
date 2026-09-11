use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;

use tokio::process::Command;
use tokio::sync::Mutex;

/// Installs one host route per member, instead of giving the TUN a wide prefix
/// and letting it swallow everything inside.
///
/// A `/10` on the interface captured four million addresses to serve two peers,
/// which is how we ended up intercepting other VPNs' traffic. Host routes mean
/// the worst case is a single colliding address rather than a whole range —
/// and since IPv6 is the primary family, a peer stays reachable over v6 even
/// when its v4 address is contested.
#[derive(Clone)]
pub struct Routes {
	iface: String,
	installed: Arc<Mutex<HashSet<IpAddr>>>,
}

impl Routes {
	pub fn new(iface: String) -> Self {
		Self {
			iface,
			installed: Arc::new(Mutex::new(HashSet::new())),
		}
	}

	/// Reconciles the system routing table against the addresses we want,
	/// adding and removing only the difference.
	pub async fn sync(&self, wanted: &HashSet<IpAddr>) {
		let mut installed = self.installed.lock().await;

		for addr in wanted.difference(&installed) {
			match self.add(*addr).await {
				Ok(()) => println!("route added: {addr} dev {}", self.iface),
				Err(e) => eprintln!("adding route for {addr} failed: {e}"),
			}
		}

		for addr in installed.difference(wanted) {
			if let Err(e) = self.remove(*addr).await {
				eprintln!("removing route for {addr} failed: {e}");
			}
		}

		installed.clone_from(wanted);
	}

	#[cfg(unix)]
	async fn add(&self, addr: IpAddr) -> anyhow::Result<()> {
		// `replace` rather than `add` so a leftover route from a previous run
		// is taken over instead of erroring.
		self.ip(&["route", "replace", &cidr(addr), "dev", &self.iface], addr)
			.await
	}

	#[cfg(unix)]
	async fn remove(&self, addr: IpAddr) -> anyhow::Result<()> {
		self.ip(&["route", "del", &cidr(addr), "dev", &self.iface], addr)
			.await
	}

	#[cfg(unix)]
	async fn ip(&self, args: &[&str], addr: IpAddr) -> anyhow::Result<()> {
		let family = if addr.is_ipv4() { "-4" } else { "-6" };
		run("ip", &[&[family], args].concat()).await
	}

	#[cfg(windows)]
	async fn add(&self, addr: IpAddr) -> anyhow::Result<()> {
		let family = if addr.is_ipv4() { "ipv4" } else { "ipv6" };
		// netsh errors when the route is already there; that is the state we
		// wanted, so it is not a failure.
		let result = run(
			"netsh",
			&[
				"interface",
				family,
				"add",
				"route",
				&format!("prefix={}", cidr(addr)),
				&format!("interface={}", self.iface),
				"store=active",
			],
		)
		.await;

		match result {
			Err(e) if e.to_string().contains("already exists") => Ok(()),
			other => other,
		}
	}

	#[cfg(windows)]
	async fn remove(&self, addr: IpAddr) -> anyhow::Result<()> {
		let family = if addr.is_ipv4() { "ipv4" } else { "ipv6" };
		run(
			"netsh",
			&[
				"interface",
				family,
				"delete",
				"route",
				&format!("prefix={}", cidr(addr)),
				&format!("interface={}", self.iface),
				"store=active",
			],
		)
		.await
	}
}

/// A single-address prefix: /32 for IPv4, /128 for IPv6.
fn cidr(addr: IpAddr) -> String {
	match addr {
		IpAddr::V4(v4) => format!("{v4}/{}", crate::tun::V4_PREFIX_LEN),
		IpAddr::V6(v6) => format!("{v6}/{}", crate::tun::V6_PREFIX_LEN),
	}
}

async fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
	let output = Command::new(program).args(args).output().await?;
	if output.status.success() {
		return Ok(());
	}

	// Both tools put the useful part on stderr, falling back to stdout.
	let mut message = String::from_utf8_lossy(&output.stderr).trim().to_string();
	if message.is_empty() {
		message = String::from_utf8_lossy(&output.stdout).trim().to_string();
	}
	anyhow::bail!("{program} {}: {message}", args.join(" "));
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn cidr_uses_host_prefixes_for_both_families() {
		assert_eq!(cidr("10.1.2.3".parse().unwrap()), "10.1.2.3/32");
		assert_eq!(cidr("200::1".parse().unwrap()), "200::1/128");
	}
}
