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
use std::sync::Arc;
use std::time::{Duration, Instant};

use proto::DnsRegistration;
use tokio::sync::watch;
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

/// How long shutdown waits for a registration command already in progress.
/// Normally a second or two; this only bites on one that has hung. Inside
/// systemd's 15s stop timeout, so the daemon still gets to clean up after it.
const STOP_LIMIT: Duration = Duration::from_secs(10);

/// A registration in progress: whatever is still retrying, and whether the
/// system resolver may hold something of ours that has to be handed back.
pub struct Registration {
	iface: String,
	/// Set just before the system resolver is asked to register, not once it has
	/// succeeded, so a daemon stopped halfway through adding the rule still
	/// removes it. Removing is safe when nothing was added.
	attempted: Arc<AtomicBool>,
	/// The background retry, and the signal that asks it to stop.
	retrying: Option<(JoinHandle<()>, watch::Sender<bool>)>,
}

/// Why one attempt did not register.
enum Failure {
	/// None of the mesh addresses answered the reachability probe.
	Unreachable,
	/// An address answered, but the system resolver could not be pointed at it.
	Register(resolv::Error),
}

/// How a background retry run ended.
#[derive(Debug, PartialEq, Eq)]
enum Outcome<T> {
	/// Registered with `target`, after `retries` retries.
	Succeeded { retries: u32, target: T },
	/// Asked to stop before it registered.
	Stopped,
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
		state
			.set_dns(DnsRegistration::Unavailable {
				fallback: Some(fallback),
				remedy: None,
			})
			.await;
		return Registration { iface, attempted, retrying: None };
	}

	// Not cancellable, and needs not be: shutdown is only listened for once
	// startup is done, so this always finishes before a stop is handled.
	let failure = match attempt(&iface, &candidates, &attempted).await {
		Ok(server) => {
			crate::info!("registered *.{ZONE} with the system resolver via {server}");
			state.set_dns(DnsRegistration::Registered).await;
			return Registration { iface, attempted, retrying: None };
		}
		Err(failure) => failure,
	};
	report_first(&failure, &candidates, &fallback);

	state.set_dns(reported(&failure, &fallback)).await;

	let (stop, stop_rx) = watch::channel(false);
	let retrying = tokio::spawn({
		let iface = iface.clone();
		let attempted = attempted.clone();
		async move {
			let started = Instant::now();

			let outcome = retry_until_stopped(
				|| dns::reachable_server(&candidates),
				|server| {
					let (iface, attempted) = (&iface, &attempted);
					async move {
						attempted.store(true, Ordering::SeqCst);
						// Silent on failure: the first one was explained in full,
						// and a machine where this never works should not add a
						// line to the log every minute.
						resolv::register(iface, server).await.is_ok()
					}
				},
				tokio::time::sleep,
				stop_rx,
			)
			.await;

			if let Outcome::Succeeded { retries, target } = outcome {
				crate::info!(
					"registered *.{ZONE} with the system resolver via {target} after {} attempts ({}s)",
					retries.saturating_add(1),
					started.elapsed().as_secs()
				);
				state.set_dns(DnsRegistration::Registered).await;
			}
		}
	});

	Registration {
		iface,
		attempted,
		retrying: Some((retrying, stop)),
	}
}

impl Registration {
	/// Stops retrying, then hands the zone back if the system resolver may hold
	/// it. On Linux the per-link settings would vanish with the interface anyway;
	/// on Windows the NRPT rule is in the registry and would outlive us.
	///
	/// A registration already in progress is waited for rather than cut off, so
	/// the removal below always comes after it and cannot be undone by it.
	pub async fn stop(self) {
		if let Some((task, stop)) = self.retrying {
			if !wait_for_stop(task, stop, STOP_LIMIT).await {
				crate::warn!(
					"warning: registering *.{ZONE} was still running {}s into shutdown and was \
					 cancelled; if it had already asked the system to add the rule, that rule may \
					 outlive this run, and the next start clears it",
					STOP_LIMIT.as_secs()
				);
			}
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

/// What `status` should say after an attempt has failed.
///
/// A system resolver that is not there is not a failure that waiting fixes, so
/// it is reported as what it is — with the one line saying what to do about it —
/// rather than as a retry in progress. Reporting the two alike is what let a
/// machine that had never registered the zone at all look like one that was two
/// seconds from succeeding.
///
/// The retry keeps running underneath either way. Someone enabling the resolver
/// is exactly what it is there to notice, and a daemon that only found out on
/// its next restart is how this stayed invisible in the first place — so the
/// state can still go to `Registered` on its own, it just no longer claims to be
/// on its way there.
fn reported(failure: &Failure, fallback: &str) -> DnsRegistration {
	match failure {
		Failure::Register(resolv::Error::NoResolver { remedy, .. }) => {
			DnsRegistration::Unavailable {
				fallback: Some(fallback.to_string()),
				remedy: remedy.map(str::to_string),
			}
		}
		Failure::Unreachable | Failure::Register(resolv::Error::Transient(_)) => {
			DnsRegistration::Retrying { fallback: fallback.to_string() }
		}
	}
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
		// Named in full rather than left to the reader: this is the one failure
		// here that nobody can wait out, and the line that says so was the line
		// missing while an Arch box quietly resolved no names at all.
		Failure::Register(error @ resolv::Error::NoResolver { remedy, .. }) => crate::warn!(
			"warning: could not register *.{ZONE} with the system resolver: {error}\n\
			 {}\
			 names resolve via {fallback} until then; the daemon keeps watching in case that changes",
			remedy.map(|remedy| format!("{remedy}\n")).unwrap_or_default()
		),
		Failure::Register(error) => crate::warn!(
			"warning: could not register *.{ZONE} with the system resolver: {error}\n\
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

/// Retries until registered or asked to stop, waiting `delay` before each try.
/// Never gives up on its own.
///
/// A stop is honoured while waiting and while probing, neither of which changes
/// anything — but never during `register`. A registration already handed to the
/// system has to be allowed to land, or it could land after shutdown has removed
/// the rule and leave one pointing at a daemon that is gone. Once it returns,
/// the stop is honoured before anything else is tried.
///
/// Waiting, probing and registering are parameters so tests can drive every
/// step without a network, a system resolver or a clock.
async fn retry_until_stopped<T, P, PF, R, RF, S, SF>(
	mut probe: P,
	mut register: R,
	mut sleep: S,
	mut stop: watch::Receiver<bool>,
) -> Outcome<T>
where
	T: Copy,
	P: FnMut() -> PF,
	PF: Future<Output = Option<T>>,
	R: FnMut(T) -> RF,
	RF: Future<Output = bool>,
	S: FnMut(Duration) -> SF,
	SF: Future<Output = ()>,
{
	let mut retry = 1;
	loop {
		if unless_stopped(&mut stop, sleep(delay(retry))).await.is_none() {
			return Outcome::Stopped;
		}
		let Some(found) = unless_stopped(&mut stop, probe()).await else {
			return Outcome::Stopped;
		};

		if let Some(target) = found {
			// The last point a stop can be honoured before something changes on
			// the system.
			if stop_requested(&stop) {
				return Outcome::Stopped;
			}
			if register(target).await {
				return Outcome::Succeeded { retries: retry, target };
			}
		}
		retry = retry.saturating_add(1);
	}
}

/// Runs `work` unless a stop is asked for first. `None` means it was stopped.
async fn unless_stopped<F: Future>(stop: &mut watch::Receiver<bool>, work: F) -> Option<F::Output> {
	tokio::select! {
		// Checked first, so a stop that is already in is never beaten by work
		// that happens to be ready at the same moment.
		biased;
		() = stopped(stop) => None,
		output = work => Some(output),
	}
}

/// Resolves once a stop has been asked for, or once nobody is left who could ask:
/// a dropped sender means the registration it belonged to is gone.
async fn stopped(stop: &mut watch::Receiver<bool>) {
	while !*stop.borrow_and_update() {
		if stop.changed().await.is_err() {
			return;
		}
	}
}

/// The same question without waiting: has a stop been asked for, or is there no
/// one left to ask?
fn stop_requested(stop: &watch::Receiver<bool>) -> bool {
	*stop.borrow() || stop.has_changed().is_err()
}

/// Asks a background retry to stop and waits for it, but not past `limit`.
/// Returns whether it finished on its own; if it did not, it has been cancelled.
async fn wait_for_stop(task: JoinHandle<()>, stop: watch::Sender<bool>, limit: Duration) -> bool {
	// An error here only means the task has already finished and let go of its
	// receiver, which is what stopping was for anyway.
	let _ = stop.send(true);
	let abort = task.abort_handle();

	match tokio::time::timeout(limit, task).await {
		Ok(_) => true,
		Err(_) => {
			abort.abort();
			false
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::cell::{Cell, RefCell};

	const FALLBACK: &str = "127.0.0.1:5354";

	/// The remedy the Linux path carries. Only the routing matters here, not the
	/// wording, so this stands in for it.
	const REMEDY: &str = "enable systemd-resolved";

	fn no_resolver(remedy: Option<&'static str>) -> Failure {
		Failure::Register(resolv::Error::NoResolver {
			source: anyhow::anyhow!("resolvectl is missing or systemd-resolved is not running"),
			remedy,
		})
	}

	#[test]
	fn a_machine_with_no_system_resolver_is_not_reported_as_retrying() {
		// The failure this whole split exists for. An Arch box that ships
		// systemd-resolved without enabling it used to report exactly what an
		// interface still settling reports, so the one case that needed a person
		// to do something looked like the one that fixes itself.
		let reported = reported(&no_resolver(Some(REMEDY)), FALLBACK);

		assert_eq!(
			reported,
			DnsRegistration::Unavailable {
				fallback: Some(FALLBACK.to_string()),
				remedy: Some(REMEDY.to_string()),
			}
		);
	}

	#[test]
	fn a_platform_with_no_remedy_to_offer_still_reports_the_state() {
		// macOS: `resolvectl` is missing for a reason nobody can act on. The
		// state is still the truth, there is just nothing to tell them to do.
		assert_eq!(
			reported(&no_resolver(None), FALLBACK),
			DnsRegistration::Unavailable {
				fallback: Some(FALLBACK.to_string()),
				remedy: None,
			}
		);
	}

	#[test]
	fn a_failure_another_attempt_could_survive_is_still_reported_as_retrying() {
		let transient = Failure::Register(resolv::Error::Transient(anyhow::anyhow!(
			"resolvectl domain quix ~quix: Unknown interface"
		)));

		assert_eq!(
			reported(&transient, FALLBACK),
			DnsRegistration::Retrying { fallback: FALLBACK.to_string() }
		);
	}

	#[test]
	fn an_address_nothing_answered_on_is_reported_as_retrying() {
		// A just-created interface does this and then stops doing it.
		assert_eq!(
			reported(&Failure::Unreachable, FALLBACK),
			DnsRegistration::Retrying { fallback: FALLBACK.to_string() }
		);
	}

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
		let (_never_sent, stop) = watch::channel(false);
		let registrations = Cell::new(0u32);
		let waits = RefCell::new(Vec::new());

		let outcome = retry_until_stopped(
			|| async { Some("server") },
			|_| {
				registrations.set(registrations.get() + 1);
				let done = registrations.get() >= 500;
				async move { done }
			},
			|wait| {
				waits.borrow_mut().push(wait);
				async {}
			},
			stop,
		)
		.await;

		assert_eq!(outcome, Outcome::Succeeded { retries: 500, target: "server" });
		let waits = waits.into_inner();
		assert_eq!(waits.len(), 500, "a wait before every retry");
		assert_eq!(waits[..6], [2, 4, 8, 16, 32, 60].map(Duration::from_secs));
		assert!(waits[6..].iter().all(|wait| *wait == Duration::from_secs(60)));
	}

	#[tokio::test]
	async fn it_stops_at_the_first_success() {
		let (_never_sent, stop) = watch::channel(false);
		let registrations = Cell::new(0u32);

		let outcome = retry_until_stopped(
			|| async { Some("server") },
			|_| {
				registrations.set(registrations.get() + 1);
				async { true }
			},
			|_| async {},
			stop,
		)
		.await;

		assert_eq!(outcome, Outcome::Succeeded { retries: 1, target: "server" });
		assert_eq!(registrations.get(), 1, "nothing more once it has worked");
	}

	#[tokio::test]
	async fn nothing_is_registered_until_an_address_answers() {
		// Pointing the system at an address nobody answers on is worse than not
		// registering at all: every lookup would time out instead of failing.
		let (_never_sent, stop) = watch::channel(false);
		let probes = Cell::new(0u32);
		let registrations = Cell::new(0u32);

		let outcome = retry_until_stopped(
			|| {
				probes.set(probes.get() + 1);
				let found = (probes.get() >= 3).then_some("server");
				async move { found }
			},
			|_| {
				registrations.set(registrations.get() + 1);
				async { true }
			},
			|_| async {},
			stop,
		)
		.await;

		assert_eq!(outcome, Outcome::Succeeded { retries: 3, target: "server" });
		assert_eq!(registrations.get(), 1, "only once something answered");
	}

	#[tokio::test]
	async fn a_stop_during_a_wait_ends_the_run_without_another_attempt() {
		let (stop_tx, stop) = watch::channel(false);
		let probes = Cell::new(0u32);

		let run = retry_until_stopped(
			|| {
				probes.set(probes.get() + 1);
				async { Some("server") }
			},
			|_| async { true },
			// A wait that would never end on its own.
			|_| std::future::pending::<()>(),
			stop,
		);
		let stopper = async {
			tokio::task::yield_now().await;
			stop_tx.send(true).unwrap();
		};

		let (outcome, ()) = tokio::join!(run, stopper);

		assert_eq!(outcome, Outcome::Stopped);
		assert_eq!(probes.get(), 0, "nothing was attempted after the stop");
	}

	#[tokio::test]
	async fn a_stop_while_registering_waits_for_the_registration_to_land() {
		// The race this exists for: a registration already handed to the system
		// resolver landing after shutdown has removed the rule, leaving a rule
		// that points at a daemon that is gone.
		let (stop_tx, stop) = watch::channel(false);
		let (release, released) = tokio::sync::oneshot::channel::<()>();
		let released = Cell::new(Some(released));
		let registering = Cell::new(false);
		let registrations = Cell::new(0u32);
		let finished = Cell::new(false);

		let run = async {
			let outcome = retry_until_stopped(
				|| async { Some("server") },
				|_| {
					registrations.set(registrations.get() + 1);
					registering.set(true);
					let released = released.take();
					async move {
						if let Some(released) = released {
							let _ = released.await;
						}
						// Failed, so without the stop there would be another try.
						false
					}
				},
				|_| async {},
				stop,
			)
			.await;
			finished.set(true);
			outcome
		};

		let stopper = async {
			while !registering.get() {
				tokio::task::yield_now().await;
			}
			stop_tx.send(true).unwrap();
			for _ in 0..20 {
				tokio::task::yield_now().await;
			}
			assert!(!finished.get(), "must not return while a registration is in flight");
			release.send(()).unwrap();
		};

		let (outcome, ()) = tokio::join!(run, stopper);

		assert_eq!(outcome, Outcome::Stopped);
		assert_eq!(registrations.get(), 1, "nothing is attempted once stop was asked for");
	}

	#[tokio::test]
	async fn stopping_waits_for_a_task_that_honours_the_stop() {
		let (stop_tx, mut stop_rx) = watch::channel(false);
		let task = tokio::spawn(async move { stopped(&mut stop_rx).await });

		assert!(wait_for_stop(task, stop_tx, Duration::from_secs(5)).await);
	}

	#[tokio::test]
	async fn stopping_a_task_that_already_finished_is_a_no_op() {
		let (stop_tx, _stop_rx) = watch::channel(false);
		let task = tokio::spawn(async {});
		tokio::task::yield_now().await;

		assert!(wait_for_stop(task, stop_tx, Duration::from_secs(5)).await);
	}

	#[tokio::test]
	async fn a_task_that_does_not_stop_in_time_is_cancelled() {
		// The last resort for a registration command that hangs: shutdown cannot
		// wait on it forever.
		let (stop_tx, _stop_rx) = watch::channel(false);
		let task = tokio::spawn(std::future::pending::<()>());
		let abort = task.abort_handle();

		assert!(!wait_for_stop(task, stop_tx, Duration::from_millis(50)).await);

		for _ in 0..100 {
			if abort.is_finished() {
				break;
			}
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
		assert!(abort.is_finished(), "cancelled rather than left running");
	}
}
