//! Diagnoses whether one endpoint can reach another knowing only its id —
//! which is all an invite code carries, and so exactly what `quix join` needs.
//!
//! Runs as a normal user (no TUN, no root):
//!
//!     cargo run -p quix-daemon --example discovery-check
//!
//! Binds two endpoints, then has B dial A by id alone, reporting how long
//! discovery plus the handshake took.

use std::time::{Duration, Instant};

use anyhow::Result;
use iroh::endpoint::{presets, Connection};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::Endpoint;

const ALPN: &[u8] = b"quix-discovery-check/0";
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct Accept;

impl ProtocolHandler for Accept {
	async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
		println!("  [A] accepted a connection from {}", connection.remote_id());
		connection.closed().await;
		Ok(())
	}
}

#[tokio::main]
async fn main() -> Result<()> {
	let a = Endpoint::builder(presets::N0).bind().await?;
	let b = Endpoint::builder(presets::N0).bind().await?;

	println!("A id: {}", a.id());
	println!("B id: {}", b.id());

	let _router = Router::builder(a.clone()).accept(ALPN, Accept).spawn();

	// Publishing to pkarr can't happen until the endpoint has a relay, so a
	// dial before that resolves nothing.
	print!("\nwaiting for both endpoints to come online... ");
	let started = Instant::now();
	tokio::join!(a.online(), b.online());
	println!("{:.1}s", started.elapsed().as_secs_f64());
	println!("A addr: {:?}", a.addr());

	// An invite carries only the coordinator's id, so this is the exact
	// resolution path a join depends on.
	println!("\nB dialing A by id alone (timeout {}s)...", TIMEOUT.as_secs());
	let started = Instant::now();

	match tokio::time::timeout(TIMEOUT, b.connect(a.id(), ALPN)).await {
		Ok(Ok(conn)) => {
			println!("✓ connected in {:.1}s", started.elapsed().as_secs_f64());
			conn.close(0u32.into(), b"done");
			println!("\ndiscovery works — a join failure is not this.");
		}
		Ok(Err(e)) => {
			println!("✗ failed after {:.1}s", started.elapsed().as_secs_f64());
			println!("  {e:#}");
			println!("\ndiscovery is the problem: B could not resolve A's id.");
		}
		Err(_) => {
			println!("✗ timed out after {}s", TIMEOUT.as_secs());
			println!("\ndiscovery is the problem: resolution never completed.");
		}
	}

	Ok(())
}
