//! The two halves of a transfer, each a relay between a local CLI and the
//! `quix-file/0` stream to the other node.
//!
//! On the wire an offer runs:
//!
//! ```text
//! sender                              receiver
//!   Offer { id, name, size, ttl }  →
//!                                  ←  Queued | Reject { reason }
//!            … waiting …
//!                                  ←  Accept | Reject
//!   Data…, Hash, End               →
//!                                  ←  Committed   (the receiving CLI has the
//!                                                  file in place, verified)
//!   close
//! ```
//!
//! Every message is a [`Frame`], control messages as JSON. Anything that ends
//! an offer early does so by closing the connection with a reason, which is
//! what the other side reports.

use std::time::Duration;

use anyhow::Result;
use iroh::endpoint::{Connection, ConnectionError};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::EndpointId;
use proto::filename::{self, Platform};
use proto::frame::{self, Frame, FrameReader};
use proto::{FileEvent, OfferState, Response};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

use super::{clamp_ttl, close, Files, Link, Outgoing, FILE_ALPN};

/// How long to wait for the other daemon to pick up, and to answer an offer
/// with whether it was queued. Neither involves a human.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the side that spoke last waits for the other to hang up, so its
/// final message is delivered rather than lost to its own close.
const LINGER: Duration = Duration::from_secs(10);

/// Control messages between the two daemons.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub(super) enum Wire {
	Offer {
		/// The sender's id for the offer. Only ever logged by the receiver,
		/// which shows its own.
		id: String,
		name: String,
		size: u64,
		/// Seconds remaining rather than a deadline, so the two clocks never
		/// have to agree.
		ttl_secs: u64,
	},
	/// The offer is listed on the receiver and waiting for its user.
	Queued,
	Accept,
	Reject {
		/// Set when the receiving daemon refused it; absent when a person did.
		#[serde(default)]
		reason: Option<String>,
	},
	Committed,
}

/// Accepts offers from other members.
#[derive(Debug, Clone)]
pub struct FileHandler {
	pub files: Files,
}

impl ProtocolHandler for FileHandler {
	async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
		self.files.receive_offer(conn).await;
		Ok(())
	}
}

impl Files {
	/// Receiving side of an offer: check it, list it, and hold its stream until
	/// it is answered, withdrawn or lapses.
	async fn receive_offer(&self, conn: Connection) {
		let peer = conn.remote_id();

		// The same gate as the data plane, before anything is read.
		if !self.is_member(&peer) {
			crate::info!("refused a file offer from non-member {peer}");
			conn.close(close::NOT_MEMBER.into(), b"not a member of this network");
			return;
		}

		let (mut send, recv) = match tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi()).await
		{
			Ok(Ok(streams)) => streams,
			_ => return,
		};
		let mut recv = FrameReader::spawn(recv);

		let (sender_id, name, size, ttl_secs) =
			match tokio::time::timeout(HANDSHAKE_TIMEOUT, recv.next()).await {
				Ok(Ok(Frame::Control(json))) => match Frame::parse(&json) {
					Ok(Wire::Offer {
						id,
						name,
						size,
						ttl_secs,
					}) => (id, name, size, ttl_secs),
					_ => return conn.close(close::FAILED.into(), b"expected an offer"),
				},
				_ => return conn.close(close::FAILED.into(), b"expected an offer"),
			};

		let admitted = filename::check(&name, Platform::current()).and_then(|()| {
			self.lock().admit(
				peer,
				name.clone(),
				size,
				clamp_ttl(ttl_secs),
				conn.clone(),
				Instant::now(),
			)
		});
		let id = match admitted {
			Ok(id) => id,
			Err(reason) => {
				crate::info!("refused offer of {name:?} from {peer}: {reason}");
				let refusal = Frame::control(&Wire::Reject {
					reason: Some(reason),
				});
				if frame::write(&mut send, &refusal).await.is_ok() {
					let _ = send.finish();
					linger(&conn).await;
				}
				return;
			}
		};

		if frame::write(&mut send, &Frame::control(&Wire::Queued))
			.await
			.is_err()
		{
			self.lock().incoming.remove(&id);
			return;
		}
		let expires_at = match self.lock().incoming.get_mut(&id) {
			Some(offer) => {
				offer.link = Some(Link { send, recv });
				offer.expires_at
			}
			// Removed in the meantime, because the peer left the roster.
			None => return,
		};
		crate::info!(
			"offer {id} from {}: {name} ({size} bytes), sender's id {sender_id}",
			self.display(&peer)
		);

		// From here the offer is the registry's. This task only watches for it
		// ending without an answer: withdrawn, or out of time.
		let from = self.display(&peer);
		tokio::select! {
			_ = conn.closed() => {
				let why = format!("{from} is no longer available (the offer was withdrawn)");
				if self.bury_incoming(&id, &conn, why) {
					crate::info!("offer {id} was withdrawn by its sender");
				}
			}
			_ = tokio::time::sleep_until(expires_at) => {
				if self.bury_incoming(&id, &conn, format!("the offer from {from} expired")) {
					crate::info!("offer {id} expired");
					conn.close(close::EXPIRED.into(), b"the offer expired");
				}
			}
		}
	}

	/// Removes an incoming offer that ended unanswered, remembering why. Only
	/// the offer this connection made: a stale watcher must never take another.
	fn bury_incoming(&self, id: &str, conn: &Connection, why: String) -> bool {
		let mut registry = self.lock();
		match registry.incoming.get(id) {
			Some(offer) if offer.conn.stable_id() == conn.stable_id() => {
				registry.incoming.remove(id);
				registry.gone.insert(id.to_string(), (Instant::now(), why));
				true
			}
			_ => false,
		}
	}

	/// Removes an incoming offer that someone is answering, with its streams.
	fn take_answerable(&self, id: &str) -> Result<(super::Incoming, Link), String> {
		let mut registry = self.lock();
		let now = Instant::now();
		registry.forget_history(now);
		match registry.incoming.get(id) {
			Some(offer) if offer.link.is_some() && offer.expires_at > now => {}
			// Its watcher has not got to it yet, but its time is up.
			Some(offer) if offer.link.is_some() => return Err("that offer has expired".to_string()),
			_ => {
				if let Some((_, why)) = registry.gone.get(id) {
					return Err(why.clone());
				}
				return Err(format!(
					"no pending offer {id} — `quix file list` shows what is waiting"
				));
			}
		}
		let mut offer = registry.incoming.remove(id).expect("checked above");
		let link = offer.link.take().expect("checked above");
		Ok((offer, link))
	}

	/// `quix file reject`: tell the sender no, and forget the offer.
	pub async fn reject(&self, id: &str) -> Response {
		let (offer, mut link) = match self.take_answerable(id) {
			Ok(taken) => taken,
			Err(message) => return Response::Error { message },
		};
		let from = self.display(&offer.peer);
		crate::info!("offer {id} from {from} rejected");

		// Best-effort: the sender learns either way, from this or from the
		// connection closing, and there is nothing to keep in either case.
		let conn = offer.conn.clone();
		tokio::spawn(async move {
			let refusal = Frame::control(&Wire::Reject { reason: None });
			if frame::write(&mut link.send, &refusal).await.is_ok() {
				let _ = link.send.finish();
				linger(&conn).await;
			}
			conn.close(close::REJECTED.into(), b"rejected");
		});

		Response::FileRejected {
			id: id.to_string(),
			name: offer.name,
			from,
		}
	}

	/// `quix file send`, from the moment the target is known: deliver the
	/// offer, hold the CLI until it is answered, then relay the file.
	///
	/// Talks to the CLI through `cli` (frames it sends) and `out` (everything
	/// said back). Returns once the offer has reached any end.
	pub async fn send<W: AsyncWrite + Unpin>(
		&self,
		peer: EndpointId,
		name: String,
		size: u64,
		ttl_secs: u64,
		mut cli: FrameReader,
		out: &mut W,
	) -> Result<()> {
		let ttl = clamp_ttl(ttl_secs);
		let to = self.display(&peer);

		// Checked against our own rules too: nothing this node's CLI sends
		// legitimately has a separator in it, so one that does is refused here
		// rather than left for the receiver to judge.
		if let Err(message) = filename::check(&name, Platform::current()) {
			return respond(out, &Response::Error { message }).await;
		}
		if !self.is_member(&peer) {
			let message = format!("{to} is not a member of this network");
			return respond(out, &Response::Error { message }).await;
		}

		// Listed from the start, so `quix file list` shows an offer that is
		// still being delivered. One that never arrives is taken back out: the
		// CLI reports it, and it was never an offer anyone could answer.
		let id = {
			let mut registry = self.lock();
			registry.forget_history(Instant::now());
			let id = registry.new_id();
			registry.outgoing.insert(
				id.clone(),
				Outgoing {
					peer,
					to: to.clone(),
					name: name.clone(),
					size,
					state: OfferState::Offered,
					created_at: Instant::now(),
					expires_at: Some(Instant::now() + ttl),
					finished_at: None,
					detail: None,
					conn: None,
				},
			);
			id
		};

		let offered = self.deliver_offer(peer, &id, &to, &name, size, ttl).await;
		let (conn, mut link) = match offered {
			Ok(offered) => offered,
			Err(message) => {
				self.lock().outgoing.remove(&id);
				return respond(out, &Response::Error { message }).await;
			}
		};

		// The window starts once the receiver has the offer, which is also when
		// its own copy of the window starts.
		let expires_at = Instant::now() + ttl;
		match self.lock().outgoing.get_mut(&id) {
			Some(offer) => {
				offer.expires_at = Some(expires_at);
				offer.conn = Some(conn.clone());
			}
			None => return Ok(()),
		}
		// The roster may have changed while the offer was on its way.
		if !self.is_member(&peer) {
			conn.close(close::NOT_MEMBER.into(), b"removed from the network");
		}
		crate::info!("offered {name} ({size} bytes) to {to} as {id}");

		// Whatever happens below, the entry must not stay "offered" or
		// "transferring" after this function is gone — not even if the task is
		// cancelled because the daemon is stopping.
		let mut outcome = Outcome::new(self, &id, &conn);

		let offered = Response::FileOffered {
			id: id.clone(),
			to: to.clone(),
			expires_in_secs: ttl.as_secs(),
		};
		if respond(out, &offered).await.is_err() {
			// Gone before it even heard the offer was made.
			outcome.finish(
				OfferState::Cancelled,
				None,
				close::CANCELLED,
				"the sender cancelled",
			);
			return Ok(());
		}

		// Waiting for an answer. The CLI sends nothing now, so anything from it —
		// including hanging up — is the user withdrawing the offer.
		let answer = tokio::select! {
			answer = link.recv.next() => answer,
			_ = cli.next() => {
				outcome.finish(OfferState::Cancelled, None, close::CANCELLED, "the sender cancelled");
				return Ok(());
			}
			_ = tokio::time::sleep_until(expires_at) => {
				outcome.finish(OfferState::Expired, None, close::EXPIRED, "the offer expired");
				return event(out, &FileEvent::Expired).await;
			}
		};

		match answer.map(|frame| control(&frame)) {
			Ok(Some(Wire::Accept)) => {}
			Ok(Some(Wire::Reject { .. })) => {
				outcome.finish(OfferState::Rejected, None, close::REJECTED, "rejected");
				return event(out, &FileEvent::Rejected).await;
			}
			// The receiver ended it. Its window started a moment before ours —
			// it had the offer before we heard it was queued — so it usually
			// notices expiry first, and says so in how it closes.
			_ => match closed_with(&conn).await {
				Some(close::EXPIRED) => {
					outcome.finish(
						OfferState::Expired,
						None,
						close::EXPIRED,
						"the offer expired",
					);
					return event(out, &FileEvent::Expired).await;
				}
				Some(close::REJECTED) => {
					outcome.finish(OfferState::Rejected, None, close::REJECTED, "rejected");
					return event(out, &FileEvent::Rejected).await;
				}
				_ => {
					let reason = self.why(&conn, &peer).await;
					outcome.finish(
						OfferState::Failed,
						Some(reason.clone()),
						close::FAILED,
						&reason,
					);
					return error(out, &format!("{to} is no longer available: {reason}")).await;
				}
			},
		}

		crate::info!("{to} accepted {id}");
		self.lock()
			.set_state(&id, OfferState::Transferring, None, Instant::now());
		event(out, &FileEvent::Accepted).await?;

		// Relaying. Both sides are watched: the CLI for the next frame, and the
		// receiver for anything it says before the end, which is only ever why
		// it is giving up.
		let mut sent: u64 = 0;
		loop {
			tokio::select! {
				next = cli.next() => match next {
					Ok(Frame::Data(data)) => {
						sent += data.len() as u64;
						if sent > size {
							let reason = "the file grew while it was being sent";
							outcome.finish(OfferState::Failed, Some(reason.into()), close::FAILED, reason);
							return error(out, reason).await;
						}
						if frame::write(&mut link.send, &Frame::Data(data)).await.is_err() {
							return self.lost(&mut outcome, &conn, &peer, &to, out).await;
						}
					}
					Ok(Frame::Hash(hash)) => {
						if frame::write(&mut link.send, &Frame::Hash(hash)).await.is_err() {
							return self.lost(&mut outcome, &conn, &peer, &to, out).await;
						}
					}
					Ok(Frame::End) => {
						if sent != size {
							let reason = "the file changed size while it was being sent";
							outcome.finish(OfferState::Failed, Some(reason.into()), close::FAILED, reason);
							return error(out, reason).await;
						}
						if frame::write(&mut link.send, &Frame::End).await.is_err() {
							return self.lost(&mut outcome, &conn, &peer, &to, out).await;
						}
						break;
					}
					// The CLI could not read its own file.
					Ok(Frame::Error(reason)) => {
						let reason = format!("the sender could not read the file: {reason}");
						outcome.finish(OfferState::Failed, Some(reason.clone()), close::FAILED, &reason);
						return Ok(());
					}
					Ok(Frame::Control(_)) => {
						let reason = "the sending CLI broke protocol";
						outcome.finish(OfferState::Failed, Some(reason.into()), close::FAILED, reason);
						return error(out, reason).await;
					}
					Err(_) => {
						outcome.finish(OfferState::Cancelled, None, close::CANCELLED, "the sender cancelled");
						return Ok(());
					}
				},
				said = link.recv.next() => {
					let reason = match said {
						Ok(Frame::Error(reason)) => reason,
						_ => self.why(&conn, &peer).await,
					};
					outcome.finish(OfferState::Failed, Some(reason.clone()), close::FAILED, &reason);
					return error(out, &format!("{to} stopped the transfer: {reason}")).await;
				}
			}
		}

		// Sent. Delivered means the receiving CLI has it in place, verified —
		// which can take a while for a large file being flushed to disk.
		match link.recv.next().await.map(|frame| (control(&frame), frame)) {
			Ok((Some(Wire::Committed), _)) => {
				crate::info!("{id} delivered to {to}");
				outcome.finish(OfferState::Done, None, close::DONE, "delivered");
				event(out, &FileEvent::Delivered).await
			}
			Ok((_, Frame::Error(reason))) => {
				outcome.finish(
					OfferState::Failed,
					Some(reason.clone()),
					close::FAILED,
					&reason,
				);
				error(out, &format!("{to} did not keep the file: {reason}")).await
			}
			_ => {
				let reason = self.why(&conn, &peer).await;
				outcome.finish(
					OfferState::Failed,
					Some(reason.clone()),
					close::FAILED,
					&reason,
				);
				error(
					out,
					&format!("{to} did not confirm the file arrived: {reason}"),
				)
				.await
			}
		}
	}

	/// The network failed under a write: say why, as the receiver put it.
	async fn lost<W: AsyncWrite + Unpin>(
		&self,
		outcome: &mut Outcome<'_>,
		conn: &Connection,
		peer: &EndpointId,
		to: &str,
		out: &mut W,
	) -> Result<()> {
		let reason = self.why(conn, peer).await;
		outcome.finish(
			OfferState::Failed,
			Some(reason.clone()),
			close::FAILED,
			&reason,
		);
		error(out, &format!("the transfer to {to} broke off: {reason}")).await
	}

	/// Why a transfer's connection ended. When this node ended it because the
	/// peer left the roster, the connection only knows it was closed locally,
	/// which would tell the user nothing.
	async fn why(&self, conn: &Connection, peer: &EndpointId) -> String {
		if !self.is_member(peer) {
			return "removed from the network".to_string();
		}
		closed_because(conn).await
	}

	/// Connects to `peer` and hands it the offer, returning once it is listed
	/// there — or why it is not.
	async fn deliver_offer(
		&self,
		peer: EndpointId,
		id: &str,
		to: &str,
		name: &str,
		size: u64,
		ttl: Duration,
	) -> std::result::Result<(Connection, Link), String> {
		let unreachable = |e: String| format!("could not reach {to}: {e}");

		let conn =
			match tokio::time::timeout(HANDSHAKE_TIMEOUT, self.endpoint.connect(peer, FILE_ALPN))
				.await
			{
				Ok(Ok(conn)) => conn,
				Ok(Err(e)) => return Err(unreachable(format!("{e:#}"))),
				Err(_) => return Err(unreachable("timed out".to_string())),
			};
		let (mut send, recv) = conn
			.open_bi()
			.await
			.map_err(|e| unreachable(e.to_string()))?;
		let mut recv = FrameReader::spawn(recv);

		let offer = Wire::Offer {
			id: id.to_string(),
			name: name.to_string(),
			size,
			ttl_secs: ttl.as_secs(),
		};
		frame::write(&mut send, &Frame::control(&offer))
			.await
			.map_err(|e| unreachable(e.to_string()))?;

		match tokio::time::timeout(HANDSHAKE_TIMEOUT, recv.next()).await {
			Ok(Ok(frame)) => match control(&frame) {
				Some(Wire::Queued) => Ok((conn, Link { send, recv })),
				Some(Wire::Reject { reason }) => {
					conn.close(close::REJECTED.into(), b"refused");
					Err(format!(
						"{to} refused the offer: {}",
						reason.unwrap_or_else(|| "no reason given".to_string())
					))
				}
				_ => Err(format!("{to} answered the offer with something unexpected")),
			},
			Ok(Err(_)) => Err(format!(
				"{to} refused the offer: {}",
				closed_because(&conn).await
			)),
			Err(_) => Err(format!("{to} did not answer the offer")),
		}
	}

	/// `quix file accept`: answer yes, and relay the file to the CLI as it
	/// arrives. Done only once the CLI says the file is in place.
	pub async fn accept<W: AsyncWrite + Unpin>(
		&self,
		id: &str,
		mut cli: FrameReader,
		out: &mut W,
	) -> Result<()> {
		let (offer, mut link) = match self.take_answerable(id) {
			Ok(taken) => taken,
			Err(message) => return respond(out, &Response::Error { message }).await,
		};
		let conn = offer.conn.clone();
		let from = self.display(&offer.peer);
		let gone = || Response::Error {
			message: format!("{from} is no longer available"),
		};

		if conn.close_reason().is_some() {
			return respond(out, &gone()).await;
		}
		if frame::write(&mut link.send, &Frame::control(&Wire::Accept))
			.await
			.is_err()
		{
			return respond(out, &gone()).await;
		}

		// Registered so that removing the sender from the roster cuts it off.
		// Removed again however this ends, including by cancellation.
		self.lock()
			.receiving
			.insert(id.to_string(), (offer.peer, conn.clone()));
		let _registered = Unregister { files: self, id };

		respond(
			out,
			&Response::FileIncoming {
				id: id.to_string(),
				from: from.clone(),
				name: offer.name.clone(),
				size: offer.size,
			},
		)
		.await?;
		crate::info!(
			"accepted {id} from {from}: {} ({} bytes)",
			offer.name,
			offer.size
		);

		let mut received: u64 = 0;
		loop {
			tokio::select! {
				next = link.recv.next() => match next {
					Ok(Frame::Data(data)) => {
						received += data.len() as u64;
						if received > offer.size {
							let reason = "the sender sent more than it offered";
							conn.close(close::FAILED.into(), reason.as_bytes());
							return error(out, reason).await;
						}
						if frame::write(out, &Frame::Data(data)).await.is_err() {
							conn.close(close::CANCELLED.into(), b"the receiver cancelled");
							return Ok(());
						}
					}
					Ok(Frame::Hash(hash)) => {
						if frame::write(out, &Frame::Hash(hash)).await.is_err() {
							conn.close(close::CANCELLED.into(), b"the receiver cancelled");
							return Ok(());
						}
					}
					Ok(Frame::End) => {
						if received != offer.size {
							let reason = "the transfer ended short of the offered size";
							conn.close(close::FAILED.into(), reason.as_bytes());
							return error(out, reason).await;
						}
						if frame::write(out, &Frame::End).await.is_err() {
							conn.close(close::CANCELLED.into(), b"the receiver cancelled");
							return Ok(());
						}
						break;
					}
					Ok(Frame::Error(reason)) => {
						return error(out, &format!("{from} stopped the transfer: {reason}")).await;
					}
					Ok(Frame::Control(_)) => {
						conn.close(close::FAILED.into(), b"unexpected message mid-transfer");
						return error(out, &format!("{from} broke protocol")).await;
					}
					Err(_) => {
						let reason = self.why(&conn, &offer.peer).await;
						let message = match received {
							0 => format!("{from} is no longer available: {reason}"),
							_ => format!("{from} went away mid-transfer: {reason}"),
						};
						return error(out, &message).await;
					}
				},
				// The CLI sends nothing until the end, so anything now is it
				// giving up: out of space, a failed write, or Ctrl+C.
				said = cli.next() => {
					let reason = match said {
						Ok(Frame::Error(reason)) => format!("the receiver could not save the file: {reason}"),
						_ => "the receiver cancelled".to_string(),
					};
					let _ = frame::write(&mut link.send, &Frame::Error(reason.clone())).await;
					conn.close(close::FAILED.into(), reason.as_bytes());
					return Ok(());
				}
			}
		}

		// Everything is with the CLI. Now it verifies, flushes and renames, and
		// only its commit makes this a delivery.
		match cli.next().await {
			Ok(Frame::End) => {
				let _ = frame::write(&mut link.send, &Frame::control(&Wire::Committed)).await;
				let _ = link.send.finish();
				crate::info!("{id} from {from} received");
				linger(&conn).await;
			}
			Ok(Frame::Error(reason)) => {
				crate::warn!("{id} from {from} was not kept: {reason}");
				let _ = frame::write(&mut link.send, &Frame::Error(reason.clone())).await;
				let _ = link.send.finish();
				linger(&conn).await;
			}
			_ => conn.close(close::CANCELLED.into(), b"the receiver cancelled"),
		}
		Ok(())
	}
}

/// Records how an outgoing offer ended, and ends its connection to match.
///
/// A guard, so the entry cannot stay "offered" or "transferring" when the task
/// running it is dropped without saying — which is exactly what happens to a
/// task when the daemon shuts down.
struct Outcome<'a> {
	files: &'a Files,
	id: &'a str,
	conn: &'a Connection,
	settled: bool,
}

impl<'a> Outcome<'a> {
	fn new(files: &'a Files, id: &'a str, conn: &'a Connection) -> Self {
		Self {
			files,
			id,
			conn,
			settled: false,
		}
	}

	fn finish(&mut self, state: OfferState, detail: Option<String>, code: u32, reason: &str) {
		self.settled = true;
		self.files
			.lock()
			.set_state(self.id, state, detail, Instant::now());
		// A connection already closed by the peer ignores this.
		self.conn.close(code.into(), reason.as_bytes());
	}
}

impl Drop for Outcome<'_> {
	fn drop(&mut self) {
		if !self.settled {
			self.finish(
				OfferState::Failed,
				Some("interrupted".to_string()),
				close::FAILED,
				"interrupted",
			);
		}
	}
}

/// Drops an accepted transfer from the registry however it ends.
struct Unregister<'a> {
	files: &'a Files,
	id: &'a str,
}

impl Drop for Unregister<'_> {
	fn drop(&mut self) {
		self.files.lock().receiving.remove(self.id);
	}
}

fn control(frame: &Frame) -> Option<Wire> {
	match frame {
		Frame::Control(json) => Frame::parse(json).ok(),
		_ => None,
	}
}

/// Why a connection ended, in the words of whoever ended it.
///
/// Called after a read or write has already failed, when the close has
/// normally landed; bounded anyway, since a stream reset alone leaves the
/// connection open.
async fn closed_because(conn: &Connection) -> String {
	let reason = match conn.close_reason() {
		Some(reason) => reason,
		None => match tokio::time::timeout(Duration::from_secs(2), conn.closed()).await {
			Ok(reason) => reason,
			Err(_) => return "the stream was reset".to_string(),
		},
	};
	match reason {
		ConnectionError::ApplicationClosed(close) => {
			String::from_utf8_lossy(&close.reason).into_owned()
		}
		ConnectionError::TimedOut => "the connection timed out".to_string(),
		ConnectionError::LocallyClosed => "closed".to_string(),
		other => other.to_string(),
	}
}

/// The code the other side closed the connection with, if it closed it.
async fn closed_with(conn: &Connection) -> Option<u32> {
	let reason = match conn.close_reason() {
		Some(reason) => reason,
		None => tokio::time::timeout(Duration::from_secs(2), conn.closed())
			.await
			.ok()?,
	};
	match reason {
		ConnectionError::ApplicationClosed(close) => {
			u32::try_from(close.error_code.into_inner()).ok()
		}
		_ => None,
	}
}

/// Waits for the other side to hang up after our last message, which is how
/// that message is known to have arrived. Bounded, since it may never.
async fn linger(conn: &Connection) {
	let _ = tokio::time::timeout(LINGER, conn.closed()).await;
}

/// Writes one JSON response line, the same shape as every other command's.
pub async fn respond<W: AsyncWrite + Unpin>(out: &mut W, response: &Response) -> Result<()> {
	let mut payload = serde_json::to_vec(response)?;
	payload.push(b'\n');
	out.write_all(&payload).await?;
	out.flush().await?;
	Ok(())
}

async fn event<W: AsyncWrite + Unpin>(out: &mut W, event: &FileEvent) -> Result<()> {
	// The CLI may already be gone — cancelled at the very moment this was
	// decided — and there is no one left to tell.
	let _ = frame::write(out, &Frame::control(event)).await;
	Ok(())
}

async fn error<W: AsyncWrite + Unpin>(out: &mut W, message: &str) -> Result<()> {
	let _ = frame::write(out, &Frame::Error(message.to_string())).await;
	Ok(())
}
