//! Keeping `.quix` registered with the operating system's resolver.
//!
//! Registration used to be tried exactly once, at startup. The check that guards
//! it — whether anything on this machine can actually reach the resolver — can
//! fail for a moment while a freshly created interface settles, and a failure
//! then was permanent: names stayed broken, with one line in the log to say so,
//! until the daemon happened to be restarted. So a failed registration is retried
//! in the background, with backoff, until it works — and `status` says so while
//! it has not.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use proto::DnsRegistration;
use tokio::task::JoinHandle;

use crate::dns::{self, ZONE};
use crate::resolv;
use crate::state::State;

/// The wait before the first retry. Doubles from here.
const FIRST_RETRY: Duration = Duration::from_secs(2);

/// The longest wait between retries. Retrying never stops: an attempt costs a
/// few datagrams to this machine, and giving up is how names ended up silently
/// broken in the first place.
const MAX_RETRY: Duration = Duration::from_secs(60);

/// A registration in progress: whatever is still retrying, and whether the
/// system resolver may hold something of ours that has to be handed back.
pub struct Registration {
	iface: String,
	/// Set just before the system resolver is asked to register, not once it has
	/// succeeded, so a daemon stopped halfway through adding the rule still
	/// removes it. Removing is safe when nothing was added.
	attempted: Arc<AtomicBool>,
	retrying: Option<JoinHandle<()>>,
}

/// Why one attempt did not register.
enum Failure {
	/// None of the mesh addresses answered the reachability probe.
	Unreachable,
	/// An address answered, but the system resolver could not be pointed at it.
	Register(anyhow::Error),
}

/// Registers `.quix` with the system resolver, or keeps trying in the background
/// until it can. Returns once the first attempt is decided either way.
///
/// `candidates` are the mesh addresses the resolver is serving on. They must
/// already be answering: they are what the reachability probe is sent to.
pub async fn start(state: State, iface: String, candidates: Vec<SocketAddr>) -> Registration {
	let attempted = Arc::new(AtomicBool::new(false));
	let fallback = fallback();

	// Nothing bound at all — `bind` has already said why for each address — so
	// there is nothing a retry could ever reach either.
	if candidates.is_empty() {
		crate::warn!(
			"warning: no mesh address could be bound, so *.{ZONE} was not registered with the \
			 system resolver\n\
			 names still resolve via {fallback}"
		);
		state.set_dns(DnsRegistration::Unavailable).await;
		return Registration { iface, attempted, retrying: None };
	}

	match attempt(&iface, &candidates, &attempted).await {
		Ok(server) => {
			crate::info!("registered *.{ZONE} with the system resolver via {server}");
			state.set_dns(DnsRegistration::Registered).await;
			return Registration { iface, attempted, retrying: None };
		}
		Err(failure) => report_first(&failure, &candidates, &fallback),
	}

	state.set_dns(DnsRegistration::Retrying { fallback }).await;

	let retrying = tokio::spawn({
		let iface = iface.clone();
		let attempted = attempted.clone();
		async move {
			let started = Instant::now();
			let via = Mutex::new(None);

			let retries = retry_forever(
				|| async {
					match attempt(&iface, &candidates, &attempted).await {
						Ok(server) => {
							*via.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(server);
							true
						}
						// Silent: the first failure was explained in full, and a
						// machine where this never works should not add a line to
						// the log every minute.
						Err(_) => false,
					}
				},
				tokio::time::sleep,
			)
			.await;

			let via = via.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner());
			crate::info!(
				"registered *.{ZONE} with the system resolver via {} after {} attempts ({}s)",
				via.map_or_else(|| "a mesh address".to_string(), |server| server.to_string()),
				retries.saturating_add(1),
				started.elapsed().as_secs()
			);
			state.set_dns(DnsRegistration::Registered).await;
		}
	});

	Registration {
		iface,
		attempted,
		retrying: Some(retrying),
	}
}

impl Registration {
	/// Stops retrying, then hands the zone back if the system resolver may hold
	/// it. On Linux the per-link settings would vanish with the interface anyway;
	/// on Windows the NRPT rule is in the registry and would outlive us.
	pub async fn stop(self) {
		if let Some(task) = self.retrying {
			task.abort();
			let _ = task.await;
		}

		if self.attempted.load(Ordering::SeqCst) {
			if let Err(e) = resolv::deregister(&self.iface).await {
				crate::warn!("warning: could not release *.{ZONE}: {e:#}");
			}
		}
	}
}

/// One try: find a mesh address that answers, and point the system resolver at
/// it. Registering an address nothing can reach is worse than registering
/// nothing — every lookup would time out instead of failing — so the probe
/// comes first.
async fn attempt(
	iface: &str,
	candidates: &[SocketAddr],
	attempted: &AtomicBool,
) -> Result<SocketAddr, Failure> {
	let server = dns::reachable_server(candidates)
		.await
		.ok_or(Failure::Unreachable)?;

	attempted.store(true, Ordering::SeqCst);
	resolv::register(iface, server)
		.await
		.map_err(Failure::Register)?;
	Ok(server)
}

/// The full explanation of why registration failed, logged once.
fn report_first(failure: &Failure, candidates: &[SocketAddr], fallback: &str) {
	match failure {
		Failure::Unreachable => {
			let addresses: Vec<String> = candidates.iter().map(ToString::to_string).collect();
			crate::warn!(
				"warning: nothing on this machine can reach the {ZONE} resolver at {}, so it is \
				 not registered with the system resolver yet\n\
				 another VPN's IPv6 leak protection unbinds IPv6 from every adapter, and its DNS \
				 leak protection filters port {}; either will do this, and so can an interface \
				 that is still settling\n\
				 retrying in the background; until then names resolve via {fallback}",
				addresses.join(" or "),
				dns::ZONE_PORT
			);
		}
		Failure::Register(e) => crate::warn!(
			"warning: could not register *.{ZONE} with the system resolver: {e:#}\n\
			 retrying in the background; until then names resolve via {fallback}"
		),
	}
}

/// Where names still resolve when the system resolver is not pointed at us.
/// Always there: it is the one endpoint the resolver refuses to start without.
fn fallback() -> String {
	dns::listen_addr()
		.map(|addr| addr.to_string())
		.unwrap_or_default()
}

/// How long to wait before retry number `retry`, counting from 1.
fn delay(retry: u32) -> Duration {
	// Saturating at every step, so a daemon left failing for months cannot
	// overflow its way into a zero wait and a busy loop.
	let factor = 1u32.checked_shl(retry.saturating_sub(1)).unwrap_or(u32::MAX);
	FIRST_RETRY
		.checked_mul(factor)
		.map_or(MAX_RETRY, |wait| wait.min(MAX_RETRY))
}

/// Retries `attempt` until it succeeds, waiting `delay` before each try. Never
/// gives up. Returns how many retries it took.
///
/// The waiting is a parameter so tests can run hundreds of retries without
/// sitting through them.
async fn retry_forever<A, AF, S, SF>(mut attempt: A, mut sleep: S) -> u32
where
	A: FnMut() -> AF,
	AF: Future<Output = bool>,
	S: FnMut(Duration) -> SF,
	SF: Future<Output = ()>,
{
	let mut retry = 1;
	loop {
		sleep(delay(retry)).await;
		if attempt().await {
			return retry;
		}
		retry = retry.saturating_add(1);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::cell::{Cell, RefCell};

	#[test]
	fn retries_back_off_by_doubling_from_two_seconds() {
		let waits: Vec<u64> = (1..=5).map(|retry| delay(retry).as_secs()).collect();
		assert_eq!(waits, [2, 4, 8, 16, 32]);
	}

	#[test]
	fn the_wait_is_capped_at_a_minute_and_stays_there() {
		assert_eq!(delay(6), Duration::from_secs(60));
		assert_eq!(delay(1_000), Duration::from_secs(60));
		// A daemon left failing for months must not overflow into a zero wait.
		assert_eq!(delay(u32::MAX), Duration::from_secs(60));
	}

	#[tokio::test]
	async fn it_keeps_trying_past_any_number_of_failures() {
		// Giving up after N attempts is what left names silently broken until
		// someone happened to restart the daemon.
		let tries = Cell::new(0u32);
		let waits = RefCell::new(Vec::new());

		let retries = retry_forever(
			|| {
				tries.set(tries.get() + 1);
				let done = tries.get() >= 500;
				async move { done }
			},
			|wait| {
				waits.borrow_mut().push(wait);
				async {}
			},
		)
		.await;

		assert_eq!(retries, 500);
		let waits = waits.into_inner();
		assert_eq!(waits.len(), 500, "a wait before every retry");
		assert_eq!(waits[..6], [2, 4, 8, 16, 32, 60].map(Duration::from_secs));
		assert!(waits[6..].iter().all(|wait| *wait == Duration::from_secs(60)));
	}

	#[tokio::test]
	async fn it_stops_at_the_first_success() {
		let tries = Cell::new(0u32);

		let retries = retry_forever(
			|| {
				tries.set(tries.get() + 1);
				async { true }
			},
			|_| async {},
		)
		.await;

		assert_eq!(retries, 1);
		assert_eq!(tries.get(), 1, "nothing more once it has worked");
	}
}
