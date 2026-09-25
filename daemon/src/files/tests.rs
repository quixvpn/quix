//! Two real endpoints on this machine, talking `quix-file/0` over real QUIC.
//!
//! No relay, no TUN and no root: each endpoint binds loopback and is told the
//! other's address directly. The CLI on each side is played by the test,
//! speaking the same frames over an in-memory pipe that the daemon speaks to
//! the real CLI over its socket.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use iroh::address_lookup::MemoryLookup;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode};
use proto::frame::{self, Frame, FrameReader};
use proto::{FileEvent, OfferState, Response, MAX_PENDING_PER_PEER};
use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::task::JoinHandle;

use super::transfer::Wire;
use super::{FileHandler, Files, FILE_ALPN};

/// Long enough for anything on loopback; short enough that a hang fails fast.
const PATIENCE: Duration = Duration::from_secs(10);

struct Node {
	files: Files,
	endpoint: Endpoint,
	_router: Router,
}

impl Node {
	async fn start(lookup: &MemoryLookup) -> Self {
		let endpoint = Endpoint::builder(presets::Minimal)
			.relay_mode(RelayMode::Disabled)
			.clear_ip_transports()
			.bind_addr("127.0.0.1:0")
			.unwrap()
			.address_lookup(lookup.clone())
			.bind()
			.await
			.expect("bind");
		let files = Files::new(endpoint.clone());
		let router = Router::builder(endpoint.clone())
			.accept(
				FILE_ALPN,
				FileHandler {
					files: files.clone(),
				},
			)
			.spawn();
		Self {
			files,
			endpoint,
			_router: router,
		}
	}

	fn id(&self) -> EndpointId {
		self.endpoint.id()
	}

	fn addr(&self) -> EndpointAddr {
		let sockets = self
			.endpoint
			.bound_sockets()
			.into_iter()
			.map(|socket| SocketAddr::new(Ipv4Addr::LOCALHOST.into(), socket.port()));
		let mut addr = EndpointAddr::new(self.id());
		for socket in sockets {
			addr = addr.with_ip_addr(socket);
		}
		addr
	}

	fn incoming(&self) -> Vec<proto::IncomingOffer> {
		self.files.list().0
	}

	fn outgoing_state(&self, id: &str) -> Option<OfferState> {
		self.files
			.list()
			.1
			.into_iter()
			.find(|o| o.id == id)
			.map(|o| o.state)
	}
}

/// Two members of the same network: "alpha" sends, "bravo" receives.
async fn pair() -> (Node, Node) {
	let lookup = MemoryLookup::new();
	let alpha = Node::start(&lookup).await;
	let bravo = Node::start(&lookup).await;
	lookup.add_endpoint_info(alpha.addr());
	lookup.add_endpoint_info(bravo.addr());
	alpha.files.set_roster([(bravo.id(), "bravo".to_string())]);
	bravo.files.set_roster([(alpha.id(), "alpha".to_string())]);
	(alpha, bravo)
}

/// The test's stand-in for a CLI connected to one daemon.
struct Cli {
	rx: BufReader<ReadHalf<DuplexStream>>,
	tx: WriteHalf<DuplexStream>,
	daemon: JoinHandle<anyhow::Result<()>>,
}

impl Cli {
	fn connect<F, Fut>(run: F) -> Self
	where
		F: FnOnce(FrameReader, WriteHalf<DuplexStream>) -> Fut,
		Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
	{
		let (daemon_side, cli_side) = tokio::io::duplex(256 * 1024);
		let (daemon_rx, daemon_tx) = tokio::io::split(daemon_side);
		let daemon = tokio::spawn(run(FrameReader::spawn(daemon_rx), daemon_tx));
		let (rx, tx) = tokio::io::split(cli_side);
		Self {
			rx: BufReader::new(rx),
			tx,
			daemon,
		}
	}

	fn send(files: &Files, to: EndpointId, name: &str, size: u64, ttl_secs: u64) -> Self {
		let files = files.clone();
		let name = name.to_string();
		Self::connect(move |cli, mut out| async move {
			files.send(to, name, size, ttl_secs, cli, &mut out).await
		})
	}

	fn accept(files: &Files, id: &str) -> Self {
		let files = files.clone();
		let id = id.to_string();
		Self::connect(move |cli, mut out| async move { files.accept(&id, cli, &mut out).await })
	}

	async fn response(&mut self) -> Response {
		let mut line = String::new();
		tokio::time::timeout(PATIENCE, self.rx.read_line(&mut line))
			.await
			.expect("the daemon answered in time")
			.expect("read the response line");
		serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("bad response {line:?}: {e}"))
	}

	async fn frame(&mut self) -> std::io::Result<Frame> {
		tokio::time::timeout(PATIENCE, frame::read(&mut self.rx))
			.await
			.expect("the daemon said something in time")
	}

	async fn event(&mut self) -> FileEvent {
		match self.frame().await.expect("a frame") {
			Frame::Control(json) => Frame::parse(&json).expect("an event"),
			other => panic!("expected an event, got {other:?}"),
		}
	}

	async fn error(&mut self) -> String {
		loop {
			match self.frame().await.expect("a frame") {
				Frame::Error(message) => return message,
				// Data already in flight when things went wrong.
				Frame::Data(_) | Frame::Hash(_) => continue,
				other => panic!("expected an error, got {other:?}"),
			}
		}
	}

	async fn write(&mut self, frame: Frame) {
		frame::write(&mut self.tx, &frame)
			.await
			.expect("the daemon is listening");
	}

	/// Streams `data` the way the real CLI does: chunks, the hash, the end.
	async fn upload(&mut self, data: &[u8]) {
		for chunk in data.chunks(frame::CHUNK) {
			self.write(Frame::Data(chunk.to_vec())).await;
		}
		self.write(Frame::Hash(*blake3::hash(data).as_bytes()))
			.await;
		self.write(Frame::End).await;
	}

	/// Collects everything up to the end frame: the data and the hash.
	async fn download(&mut self) -> (Vec<u8>, [u8; 32]) {
		let mut data = Vec::new();
		let mut hash = None;
		loop {
			match self.frame().await.expect("a frame") {
				Frame::Data(chunk) => data.extend_from_slice(&chunk),
				Frame::Hash(h) => hash = Some(h),
				Frame::End => return (data, hash.expect("a hash before the end")),
				other => panic!("unexpected {other:?} mid-download"),
			}
		}
	}

	/// Hangs up, as a CLI does when it dies or is interrupted.
	fn hang_up(self) -> JoinHandle<anyhow::Result<()>> {
		self.daemon
	}
}

/// Waits for a condition the network brings about in its own time.
async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
	let deadline = tokio::time::Instant::now() + PATIENCE;
	while !condition() {
		assert!(
			tokio::time::Instant::now() < deadline,
			"timed out waiting for: {what}"
		);
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
}

fn contents(size: usize) -> Vec<u8> {
	(0..size).map(|i| (i * 31 % 251) as u8).collect()
}

/// Makes an offer and waits until the receiver lists it, returning the
/// sender's CLI, the sender's id and the receiver's id for it.
async fn offer(
	alpha: &Node,
	bravo: &Node,
	name: &str,
	size: u64,
	ttl_secs: u64,
) -> (Cli, String, String) {
	let mut sender = Cli::send(&alpha.files, bravo.id(), name, size, ttl_secs);
	let Response::FileOffered { id, to, .. } = sender.response().await else {
		panic!("the offer should be delivered");
	};
	assert_eq!(to, "bravo");
	let listed = bravo
		.incoming()
		.into_iter()
		.find(|o| o.name == name)
		.expect("listed on the receiver");
	(sender, id, listed.id)
}

#[tokio::test]
async fn an_accepted_offer_arrives_whole_and_is_confirmed_on_both_sides() {
	let (alpha, bravo) = pair().await;
	// Several frames' worth, and not a multiple of the chunk size.
	let data = contents(3 * frame::CHUNK + 1234);

	let (mut sender, out_id, in_id) =
		offer(&alpha, &bravo, "photo.jpg", data.len() as u64, 600).await;

	let listed = &bravo.incoming()[0];
	assert_eq!(listed.from, "alpha");
	assert_eq!(listed.size, data.len() as u64);
	assert!(listed.expires_in_secs > 590 && listed.expires_in_secs <= 600);
	assert_eq!(alpha.outgoing_state(&out_id), Some(OfferState::Offered));

	let mut receiver = Cli::accept(&bravo.files, &in_id);
	match receiver.response().await {
		Response::FileIncoming {
			name, size, from, ..
		} => {
			assert_eq!(
				(name.as_str(), size, from.as_str()),
				("photo.jpg", data.len() as u64, "alpha")
			);
		}
		other => panic!("expected FileIncoming, got {other:?}"),
	}
	assert!(
		bravo.incoming().is_empty(),
		"an answered offer is no longer waiting"
	);

	assert_eq!(sender.event().await, FileEvent::Accepted);
	assert_eq!(
		alpha.outgoing_state(&out_id),
		Some(OfferState::Transferring)
	);
	sender.upload(&data).await;

	let (received, hash) = receiver.download().await;
	assert_eq!(received, data, "every byte, in order");
	assert_eq!(
		hash,
		*blake3::hash(&data).as_bytes(),
		"the sender's hash, untouched"
	);

	// Nothing is delivered until the receiving CLI commits.
	assert_eq!(
		alpha.outgoing_state(&out_id),
		Some(OfferState::Transferring)
	);
	receiver.write(Frame::End).await;

	assert_eq!(sender.event().await, FileEvent::Delivered);
	assert_eq!(alpha.outgoing_state(&out_id), Some(OfferState::Done));
}

#[tokio::test]
async fn an_empty_file_is_still_a_transfer() {
	let (alpha, bravo) = pair().await;
	let (mut sender, _, in_id) = offer(&alpha, &bravo, "empty", 0, 600).await;

	let mut receiver = Cli::accept(&bravo.files, &in_id);
	assert!(matches!(
		receiver.response().await,
		Response::FileIncoming { size: 0, .. }
	));
	assert_eq!(sender.event().await, FileEvent::Accepted);
	sender.upload(&[]).await;
	let (received, _) = receiver.download().await;
	assert!(received.is_empty());
	receiver.write(Frame::End).await;
	assert_eq!(sender.event().await, FileEvent::Delivered);
}

#[tokio::test]
async fn a_rejected_offer_moves_no_bytes_and_says_so() {
	let (alpha, bravo) = pair().await;
	let (mut sender, out_id, in_id) = offer(&alpha, &bravo, "a.txt", 10, 600).await;

	match bravo.files.reject(&in_id).await {
		Response::FileRejected { name, from, .. } => {
			assert_eq!((name.as_str(), from.as_str()), ("a.txt", "alpha"))
		}
		other => panic!("expected FileRejected, got {other:?}"),
	}
	assert!(bravo.incoming().is_empty());

	assert_eq!(sender.event().await, FileEvent::Rejected);
	assert_eq!(alpha.outgoing_state(&out_id), Some(OfferState::Rejected));

	// Answered once is answered.
	assert!(matches!(
		bravo.files.reject(&in_id).await,
		Response::Error { .. }
	));
}

#[tokio::test]
async fn an_unanswered_offer_expires_on_both_sides() {
	let (alpha, bravo) = pair().await;
	let (mut sender, out_id, in_id) = offer(&alpha, &bravo, "a.txt", 10, 1).await;

	assert_eq!(sender.event().await, FileEvent::Expired);
	assert_eq!(alpha.outgoing_state(&out_id), Some(OfferState::Expired));
	eventually("the receiver drops it", || bravo.incoming().is_empty()).await;

	let mut late = Cli::accept(&bravo.files, &in_id);
	let Response::Error { message } = late.response().await else {
		panic!("a lapsed offer cannot be accepted");
	};
	assert!(message.contains("expired"), "{message}");
}

#[tokio::test]
async fn a_peer_outside_the_network_cannot_offer_anything() {
	let (alpha, bravo) = pair().await;
	// Bravo no longer counts alpha as a member; alpha does not know that yet.
	bravo.files.set_roster([]);

	let mut sender = Cli::send(&alpha.files, bravo.id(), "a.txt", 10, 600);
	let Response::Error { message } = sender.response().await else {
		panic!("the offer must be refused");
	};
	assert!(message.contains("not a member"), "{message}");
	assert!(bravo.incoming().is_empty());
	assert!(
		alpha.files.list().1.is_empty(),
		"an offer that never arrived is not listed"
	);
}

#[tokio::test]
async fn no_offer_is_made_to_a_peer_outside_the_network() {
	let (alpha, bravo) = pair().await;
	alpha.files.set_roster([]);

	let mut sender = Cli::send(&alpha.files, bravo.id(), "a.txt", 10, 600);
	let Response::Error { message } = sender.response().await else {
		panic!("the offer must be refused");
	};
	assert!(message.contains("not a member"), "{message}");
}

#[tokio::test]
async fn one_peer_can_only_have_so_many_offers_waiting() {
	let (alpha, bravo) = pair().await;

	let mut waiting = Vec::new();
	for n in 0..MAX_PENDING_PER_PEER {
		let mut sender = Cli::send(&alpha.files, bravo.id(), &format!("f{n}"), 1, 600);
		assert!(
			matches!(sender.response().await, Response::FileOffered { .. }),
			"offer {n}"
		);
		waiting.push(sender);
	}

	let mut one_too_many = Cli::send(&alpha.files, bravo.id(), "last", 1, 600);
	let Response::Error { message } = one_too_many.response().await else {
		panic!("the offer past the limit must be refused");
	};
	assert!(
		message.contains(&MAX_PENDING_PER_PEER.to_string()),
		"{message}"
	);
	assert_eq!(bravo.incoming().len(), MAX_PENDING_PER_PEER);

	// Answering one makes room again.
	let first = bravo.incoming()[0].id.clone();
	bravo.files.reject(&first).await;
	let mut room = Cli::send(&alpha.files, bravo.id(), "room", 1, 600);
	assert!(matches!(
		room.response().await,
		Response::FileOffered { .. }
	));
}

#[tokio::test]
async fn a_sender_that_cancels_while_waiting_withdraws_the_offer() {
	let (alpha, bravo) = pair().await;
	let (sender, out_id, in_id) = offer(&alpha, &bravo, "a.txt", 10, 600).await;

	sender.hang_up().await.unwrap().unwrap();
	assert_eq!(alpha.outgoing_state(&out_id), Some(OfferState::Cancelled));
	eventually("the offer disappears from the receiver", || {
		bravo.incoming().is_empty()
	})
	.await;

	let mut late = Cli::accept(&bravo.files, &in_id);
	let Response::Error { message } = late.response().await else {
		panic!("a withdrawn offer cannot be accepted");
	};
	assert!(message.contains("no longer available"), "{message}");
}

#[tokio::test]
async fn a_sender_that_dies_mid_transfer_fails_the_receiver() {
	let (alpha, bravo) = pair().await;
	let (mut sender, out_id, in_id) =
		offer(&alpha, &bravo, "big.bin", 10 * frame::CHUNK as u64, 600).await;

	let mut receiver = Cli::accept(&bravo.files, &in_id);
	receiver.response().await;
	assert_eq!(sender.event().await, FileEvent::Accepted);
	sender.write(Frame::Data(vec![1; frame::CHUNK])).await;
	// Mid-transfer means bytes were already arriving. (A QUIC close can
	// overtake data still in flight, so without this the receiver may see none.)
	assert!(matches!(receiver.frame().await, Ok(Frame::Data(_))));

	sender.hang_up().await.unwrap().unwrap();

	let message = receiver.error().await;
	assert!(message.contains("mid-transfer"), "{message}");
	assert_eq!(alpha.outgoing_state(&out_id), Some(OfferState::Cancelled));
}

#[tokio::test]
async fn a_receiver_that_dies_mid_transfer_fails_the_sender() {
	let (alpha, bravo) = pair().await;
	let (mut sender, out_id, in_id) =
		offer(&alpha, &bravo, "big.bin", 100 * frame::CHUNK as u64, 600).await;

	let mut receiver = Cli::accept(&bravo.files, &in_id);
	receiver.response().await;
	assert_eq!(sender.event().await, FileEvent::Accepted);
	sender.write(Frame::Data(vec![1; frame::CHUNK])).await;
	receiver.frame().await.unwrap();

	receiver.hang_up().await.unwrap().unwrap();

	// The sender keeps writing until the break reaches it.
	let more = Frame::Data(vec![2; frame::CHUNK]);
	let message = tokio::time::timeout(PATIENCE, async {
		loop {
			tokio::select! {
				said = frame::read(&mut sender.rx) => match said.expect("a frame") {
					Frame::Error(message) => break message,
					other => panic!("expected an error, got {other:?}"),
				},
				_ = frame::write(&mut sender.tx, &more) => {}
			}
		}
	})
	.await
	.expect("the sender hears in time");
	assert!(message.contains("cancelled"), "{message}");
	assert_eq!(alpha.outgoing_state(&out_id), Some(OfferState::Failed));
}

#[tokio::test]
async fn a_file_the_receiver_could_not_verify_is_not_delivered() {
	let (alpha, bravo) = pair().await;
	let data = contents(5000);
	let (mut sender, out_id, in_id) = offer(&alpha, &bravo, "a.bin", data.len() as u64, 600).await;

	let mut receiver = Cli::accept(&bravo.files, &in_id);
	receiver.response().await;
	assert_eq!(sender.event().await, FileEvent::Accepted);
	sender.upload(&data).await;
	receiver.download().await;

	// What the real CLI sends when the hash does not match: no commit.
	receiver
		.write(Frame::Error("the file did not match its hash".to_string()))
		.await;

	let message = sender.error().await;
	assert!(message.contains("did not match"), "{message}");
	assert_eq!(alpha.outgoing_state(&out_id), Some(OfferState::Failed));
}

#[tokio::test]
async fn a_file_that_changes_size_while_sending_is_refused_by_the_sender() {
	let (alpha, bravo) = pair().await;
	let (mut sender, out_id, in_id) = offer(&alpha, &bravo, "log.txt", 10, 600).await;

	let mut receiver = Cli::accept(&bravo.files, &in_id);
	receiver.response().await;
	assert_eq!(sender.event().await, FileEvent::Accepted);
	sender.write(Frame::Data(vec![0; 11])).await;

	let message = sender.error().await;
	assert!(message.contains("grew"), "{message}");
	assert_eq!(alpha.outgoing_state(&out_id), Some(OfferState::Failed));
	assert!(
		!receiver.error().await.is_empty(),
		"the receiver hears it failed"
	);
}

#[tokio::test]
async fn removing_the_sender_from_the_roster_drops_its_offers() {
	let (alpha, bravo) = pair().await;
	let (mut sender, out_id, _) = offer(&alpha, &bravo, "a.txt", 10, 600).await;

	bravo.files.set_roster([]);
	assert!(bravo.incoming().is_empty(), "gone at once");

	let message = sender.error().await;
	assert!(message.contains("no longer"), "{message}");
	assert_eq!(alpha.outgoing_state(&out_id), Some(OfferState::Failed));
}

#[tokio::test]
async fn removing_the_receiver_from_the_roster_ends_the_offer() {
	let (alpha, bravo) = pair().await;
	let (mut sender, out_id, _) = offer(&alpha, &bravo, "a.txt", 10, 600).await;

	alpha.files.set_roster([]);

	let message = sender.error().await;
	assert!(message.contains("removed from the network"), "{message}");
	assert_eq!(alpha.outgoing_state(&out_id), Some(OfferState::Failed));
	eventually("the receiver drops it too", || bravo.incoming().is_empty()).await;
}

/// A sender that skips its own daemon's checks, to show the receiver makes
/// them itself.
async fn raw_offer(
	alpha: &Node,
	bravo: &Node,
	name: &str,
	size: u64,
) -> (
	iroh::endpoint::Connection,
	iroh::endpoint::SendStream,
	FrameReader,
	Wire,
) {
	let conn = alpha
		.endpoint
		.connect(bravo.id(), FILE_ALPN)
		.await
		.expect("connect");
	let (mut send, recv) = conn.open_bi().await.expect("open a stream");
	let mut recv = FrameReader::spawn(recv);
	let offer = Wire::Offer {
		id: "raw".to_string(),
		name: name.to_string(),
		size,
		ttl_secs: 600,
	};
	frame::write(&mut send, &Frame::control(&offer))
		.await
		.expect("send the offer");
	let Frame::Control(json) = recv.next().await.expect("an answer") else {
		panic!("expected a control frame");
	};
	(
		conn,
		send,
		recv,
		Frame::parse(&json).expect("a wire message"),
	)
}

#[tokio::test]
async fn the_receiver_refuses_a_name_that_is_a_path() {
	let (alpha, bravo) = pair().await;
	for name in ["../../.bashrc", "/etc/passwd", r"..\evil.dll", "", ".."] {
		let (_conn, _send, _recv, answer) = raw_offer(&alpha, &bravo, name, 1).await;
		let Wire::Reject {
			reason: Some(reason),
		} = answer
		else {
			panic!("{name:?} must be refused, got {answer:?}");
		};
		assert!(!reason.is_empty());
	}
	assert!(bravo.incoming().is_empty(), "nothing was listed");
}

#[tokio::test]
async fn the_receiver_refuses_more_bytes_than_were_offered() {
	let (alpha, bravo) = pair().await;
	let (_conn, mut send, mut recv, answer) = raw_offer(&alpha, &bravo, "small.txt", 4).await;
	assert!(matches!(answer, Wire::Queued), "{answer:?}");

	let in_id = bravo.incoming()[0].id.clone();
	let mut receiver = Cli::accept(&bravo.files, &in_id);
	receiver.response().await;
	assert!(
		matches!(recv.next().await, Ok(Frame::Control(_))),
		"accepted"
	);

	frame::write(
		&mut send,
		&Frame::Data(b"far more than four bytes".to_vec()),
	)
	.await
	.unwrap();

	let message = receiver.error().await;
	assert!(message.contains("more than it offered"), "{message}");
}
