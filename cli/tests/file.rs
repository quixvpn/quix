//! The real `quix` binary against a fake daemon on a private socket.
//!
//! What these pin down is where things happen: that a file the caller cannot
//! read, or a directory they cannot write to, fails in the CLI before the
//! daemon hears a thing — observed as the fake daemon never getting a
//! connection — and that what arrives is written by, and belongs to, the
//! caller. Unix only: the permission setups are Unix permission bits.
#![cfg(unix)]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proto::frame::{self, Frame, FrameReader};
use proto::{FileEvent, IncomingOffer, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// What the fake daemon does with one request: answer it, send some frames,
/// then note everything the CLI sends back.
struct Script {
	response: Response,
	frames: Vec<Frame>,
}

struct FakeDaemon {
	socket: PathBuf,
	connections: Arc<AtomicUsize>,
	/// Frames the CLI sent after the response, across all connections.
	heard: Arc<Mutex<Vec<Frame>>>,
	_dir: tempfile::TempDir,
}

impl FakeDaemon {
	fn start(script: impl Fn(&Request) -> Script + Send + Sync + 'static) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let socket = dir.path().join("quixd.sock");
		let listener = UnixListener::bind(&socket).unwrap();
		let connections = Arc::new(AtomicUsize::new(0));
		let heard = Arc::new(Mutex::new(Vec::new()));
		let script = Arc::new(script);

		let (count, log) = (connections.clone(), heard.clone());
		tokio::spawn(async move {
			while let Ok((stream, _)) = listener.accept().await {
				count.fetch_add(1, Ordering::SeqCst);
				let (script, log) = (script.clone(), log.clone());
				tokio::spawn(async move {
					let (rx, mut tx) = stream.into_split();
					let mut rx = BufReader::new(rx);
					let mut line = String::new();
					rx.read_line(&mut line).await.unwrap();
					let request: Request = serde_json::from_str(line.trim()).unwrap();
					let Script { response, frames } = script(&request);

					let mut payload = serde_json::to_vec(&response).unwrap();
					payload.push(b'\n');
					tx.write_all(&payload).await.unwrap();
					for frame in &frames {
						frame::write(&mut tx, frame).await.unwrap();
					}

					let mut cli = FrameReader::spawn(rx);
					while let Ok(frame) = cli.next().await {
						log.lock().unwrap().push(frame);
					}
				});
			}
		});

		Self {
			socket,
			connections,
			heard,
			_dir: dir,
		}
	}

	fn connections(&self) -> usize {
		self.connections.load(Ordering::SeqCst)
	}

	fn heard(&self) -> Vec<Frame> {
		self.heard.lock().unwrap().clone()
	}

	/// Runs `quix` with `args` from `cwd`, with no terminal attached.
	async fn quix(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
		let run = tokio::process::Command::new(env!("CARGO_BIN_EXE_quix"))
			.args(args)
			.current_dir(cwd)
			.env("QUIX_SOCKET", &self.socket)
			.stdin(std::process::Stdio::null())
			.output();
		tokio::time::timeout(Duration::from_secs(20), run)
			.await
			.expect("quix finished in time")
			.expect("quix ran")
	}
}

fn is_root() -> bool {
	unsafe { libc::geteuid() == 0 }
}

fn stderr(output: &std::process::Output) -> String {
	String::from_utf8_lossy(&output.stderr).into_owned()
}

fn offer(id: &str, name: &str, size: u64) -> IncomingOffer {
	IncomingOffer {
		id: id.to_string(),
		from: "nas".to_string(),
		name: name.to_string(),
		size,
		expires_in_secs: 570,
	}
}

/// A daemon with one offer waiting, which hands over `data` when accepted.
fn daemon_offering(name: &'static str, data: &'static [u8]) -> FakeDaemon {
	FakeDaemon::start(move |request| match request {
		Request::FileList => Script {
			response: Response::Files {
				incoming: vec![offer("0000aaaa", name, data.len() as u64)],
				outgoing: vec![],
			},
			frames: vec![],
		},
		Request::FileAccept { id } => Script {
			response: Response::FileIncoming {
				id: id.clone(),
				from: "nas".to_string(),
				name: name.to_string(),
				size: data.len() as u64,
			},
			frames: vec![
				Frame::Data(data.to_vec()),
				Frame::Hash(*blake3::hash(data).as_bytes()),
				Frame::End,
			],
		},
		other => panic!("unexpected {other:?}"),
	})
}

#[tokio::test]
async fn a_file_the_caller_cannot_read_never_reaches_the_daemon() {
	if is_root() {
		eprintln!("skipped: root reads everything");
		return;
	}
	let daemon =
		FakeDaemon::start(|request| panic!("the daemon must not be asked anything: {request:?}"));
	let dir = tempfile::tempdir().unwrap();
	let secret = dir.path().join("secret");
	std::fs::write(&secret, b"root's business").unwrap();
	std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();

	let output = daemon
		.quix(
			dir.path(),
			&["file", "send", secret.to_str().unwrap(), "nas"],
		)
		.await;

	assert!(!output.status.success());
	assert!(
		stderr(&output).contains("Permission denied"),
		"{}",
		stderr(&output)
	);
	assert_eq!(daemon.connections(), 0, "the daemon was never contacted");
}

#[tokio::test]
async fn a_directory_is_refused_before_the_daemon_is_contacted() {
	let daemon =
		FakeDaemon::start(|request| panic!("the daemon must not be asked anything: {request:?}"));
	let dir = tempfile::tempdir().unwrap();

	let output = daemon.quix(dir.path(), &["file", "send", ".", "nas"]).await;

	assert!(!output.status.success());
	assert!(stderr(&output).contains("directory"), "{}", stderr(&output));
	assert_eq!(daemon.connections(), 0);
}

#[tokio::test]
async fn accepting_into_a_directory_the_caller_cannot_write_to_never_reaches_the_daemon() {
	if is_root() {
		eprintln!("skipped: root writes everywhere");
		return;
	}
	let daemon = daemon_offering("photo.jpg", b"hello");
	let dir = tempfile::tempdir().unwrap();
	std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

	let output = daemon
		.quix(dir.path(), &["file", "accept", "0000aaaa", "--here"])
		.await;
	std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

	assert!(!output.status.success());
	assert!(
		stderr(&output).contains("Permission denied"),
		"{}",
		stderr(&output)
	);
	assert_eq!(daemon.connections(), 0, "the offer is untouched");
}

#[tokio::test]
async fn an_accepted_file_is_saved_here_verified_committed_and_owned_by_the_caller() {
	let daemon = daemon_offering("photo.jpg", b"hello");
	let dir = tempfile::tempdir().unwrap();

	let output = daemon
		.quix(dir.path(), &["file", "accept", "0000aaaa", "--here"])
		.await;
	assert!(output.status.success(), "{}", stderr(&output));

	let saved = dir.path().join("photo.jpg");
	assert_eq!(std::fs::read(&saved).unwrap(), b"hello");
	// Written by this process, so owned by whoever ran it — never the daemon's
	// account. (With a daemon running as root, the real two-node run is what
	// makes this bite; here both run as the same user.)
	let owner = std::fs::metadata(&saved).unwrap().uid();
	assert_eq!(owner, unsafe { libc::geteuid() });

	let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
	assert_eq!(entries.len(), 1, "no partial file left over");

	// Give the fake daemon a moment to log what the CLI said last.
	tokio::time::sleep(Duration::from_millis(100)).await;
	assert_eq!(
		daemon.heard(),
		[Frame::End],
		"committed once the file was in place"
	);
}

#[tokio::test]
async fn a_file_that_fails_its_hash_is_not_saved_and_the_sender_hears_why() {
	let daemon = FakeDaemon::start(|request| match request {
		Request::FileList => Script {
			response: Response::Files {
				incoming: vec![offer("0000aaaa", "photo.jpg", 5)],
				outgoing: vec![],
			},
			frames: vec![],
		},
		Request::FileAccept { id } => Script {
			response: Response::FileIncoming {
				id: id.clone(),
				from: "nas".to_string(),
				name: "photo.jpg".to_string(),
				size: 5,
			},
			frames: vec![
				Frame::Data(b"hello".to_vec()),
				Frame::Hash([0; 32]),
				Frame::End,
			],
		},
		other => panic!("unexpected {other:?}"),
	});
	let dir = tempfile::tempdir().unwrap();

	let output = daemon
		.quix(dir.path(), &["file", "accept", "0000aaaa", "--here"])
		.await;

	assert!(!output.status.success());
	assert!(stderr(&output).contains("hash"), "{}", stderr(&output));
	assert_eq!(
		std::fs::read_dir(dir.path()).unwrap().count(),
		0,
		"nothing saved, no partial file"
	);
	tokio::time::sleep(Duration::from_millis(100)).await;
	assert!(
		matches!(daemon.heard().as_slice(), [Frame::Error(_)]),
		"{:?}",
		daemon.heard()
	);
}

#[tokio::test]
async fn an_existing_file_is_kept_and_the_new_one_numbered() {
	let daemon = daemon_offering("photo.jpg", b"new");
	let dir = tempfile::tempdir().unwrap();
	std::fs::write(dir.path().join("photo.jpg"), b"old").unwrap();

	let output = daemon
		.quix(dir.path(), &["file", "accept", "0000aaaa", "--here"])
		.await;

	assert!(output.status.success(), "{}", stderr(&output));
	assert_eq!(std::fs::read(dir.path().join("photo.jpg")).unwrap(), b"old");
	assert_eq!(
		std::fs::read(dir.path().join("photo (1).jpg")).unwrap(),
		b"new"
	);
}

#[tokio::test]
async fn list_without_a_terminal_prints_a_plain_table_and_asks_nothing() {
	let daemon = FakeDaemon::start(|request| match request {
		Request::FileList => Script {
			response: Response::Files {
				incoming: vec![
					offer("0000aaaa", "photo.jpg", 2048),
					offer("0000bbbb", "notes.txt", 12),
				],
				outgoing: vec![],
			},
			frames: vec![],
		},
		other => panic!("a script's list must not act on anything: {other:?}"),
	});
	let dir = tempfile::tempdir().unwrap();

	let output = daemon.quix(dir.path(), &["file", "list"]).await;

	assert!(output.status.success(), "{}", stderr(&output));
	assert_eq!(
		String::from_utf8_lossy(&output.stdout),
		"ID\tFROM\tNAME\tSIZE\tEXPIRES\n\
		 0000aaaa\tnas\tphoto.jpg\t2.0 KiB\t9m 30s\n\
		 0000bbbb\tnas\tnotes.txt\t12 B\t9m 30s\n"
	);
	assert_eq!(
		daemon.connections(),
		1,
		"one request, nothing accepted or rejected"
	);
}

/// A daemon that takes an offer and answers it with `event`.
fn daemon_answering(event: FileEvent) -> FakeDaemon {
	FakeDaemon::start(move |request| match request {
		Request::FileSend { .. } => Script {
			response: Response::FileOffered {
				id: "0000cccc".to_string(),
				to: "nas".to_string(),
				expires_in_secs: 600,
			},
			frames: vec![Frame::control(&event)],
		},
		other => panic!("unexpected {other:?}"),
	})
}

async fn send_and_get_code(event: FileEvent) -> (Option<i32>, String) {
	let daemon = daemon_answering(event);
	let dir = tempfile::tempdir().unwrap();
	std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
	let output = daemon
		.quix(dir.path(), &["file", "send", "a.txt", "nas"])
		.await;
	(output.status.code(), stderr(&output))
}

#[tokio::test]
async fn a_rejected_offer_exits_with_its_own_code() {
	let (code, said) = send_and_get_code(FileEvent::Rejected).await;
	assert_eq!(code, Some(3), "{said}");
	assert!(said.contains("rejected"), "{said}");
}

#[tokio::test]
async fn an_expired_offer_exits_with_its_own_code() {
	let (code, said) = send_and_get_code(FileEvent::Expired).await;
	assert_eq!(code, Some(4), "{said}");
	assert!(said.contains("expired"), "{said}");
}

#[tokio::test]
async fn a_sent_file_is_streamed_with_its_hash_and_nothing_but_the_bare_name_is_sent() {
	let requests = Arc::new(Mutex::new(Vec::new()));
	let seen = requests.clone();
	let daemon = FakeDaemon::start(move |request| {
		seen.lock().unwrap().push(format!("{request:?}"));
		Script {
			response: Response::FileOffered {
				id: "0000cccc".to_string(),
				to: "nas".to_string(),
				expires_in_secs: 600,
			},
			// Accepted, and delivered as soon as it has been sent.
			frames: vec![
				Frame::control(&FileEvent::Accepted),
				Frame::control(&FileEvent::Delivered),
			],
		}
	});
	let dir = tempfile::tempdir().unwrap();
	std::fs::create_dir(dir.path().join("private")).unwrap();
	let data = vec![7u8; 200_000];
	std::fs::write(dir.path().join("private/data.bin"), &data).unwrap();

	let output = daemon
		.quix(
			dir.path(),
			&["file", "send", "private/data.bin", "10.1.2.3"],
		)
		.await;
	assert!(output.status.success(), "{}", stderr(&output));

	let request = requests.lock().unwrap()[0].clone();
	assert!(request.contains(r#"name: "data.bin""#), "{request}");
	assert!(
		!request.contains("private"),
		"no path reaches the daemon: {request}"
	);
	assert!(
		request.contains("10.1.2.3"),
		"the target is passed through as typed: {request}"
	);

	tokio::time::sleep(Duration::from_millis(100)).await;
	let heard = daemon.heard();
	let sent: Vec<u8> = heard
		.iter()
		.filter_map(|f| match f {
			Frame::Data(d) => Some(d.clone()),
			_ => None,
		})
		.flatten()
		.collect();
	assert_eq!(sent, data);
	let tail = &heard[heard.len() - 2..];
	assert_eq!(
		tail,
		[Frame::Hash(*blake3::hash(&data).as_bytes()), Frame::End]
	);
}
