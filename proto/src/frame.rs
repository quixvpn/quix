//! The binary framing a file transfer switches to after its JSON request line.
//!
//! Used on both hops that carry file contents — CLI ⇄ daemon over the local
//! socket, and daemon ⇄ daemon over a `quix-file/0` stream — so a daemon relays
//! a frame by reading it from one side and writing it to the other.
//!
//! ```text
//! frame := tag:u8  len:u32 (big-endian)  payload[len]
//! ```
//!
//! Every stream ends with an explicit [`Frame::End`], never with EOF. Named
//! pipes on Windows have no half-close, so "the other side stopped writing" is
//! indistinguishable from "the other side died"; an end frame makes a complete
//! transfer something that is said rather than inferred. EOF anywhere else is a
//! cancellation.

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest payload a frame may carry. A length over this is refused before
/// anything is allocated, so a hostile or corrupt length cannot make the reader
/// reserve four gigabytes.
pub const MAX_PAYLOAD: usize = 1 << 20;

/// How much file data a writer puts in one frame. Small enough that a slow
/// reader holds back a fast writer promptly, large enough that framing costs
/// nothing measurable.
pub const CHUNK: usize = 64 * 1024;

const TAG_DATA: u8 = b'D';
const TAG_HASH: u8 = b'H';
const TAG_END: u8 = b'E';
const TAG_CONTROL: u8 = b'C';
const TAG_ERROR: u8 = b'X';

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
	/// A run of file contents.
	Data(Vec<u8>),
	/// The BLAKE3 hash of everything sent as [`Frame::Data`], sent once after
	/// the last chunk. The sender computes it while reading, so the file is read
	/// exactly once.
	Hash([u8; 32]),
	/// The end of whatever this side had to send. After file data it closes the
	/// transfer; sent by the receiving CLI it is the commit.
	End,
	/// A JSON message, for everything that is not file contents.
	Control(Vec<u8>),
	/// The other side is giving up, and why.
	Error(String),
}

impl Frame {
	/// Builds a control frame from anything serializable.
	pub fn control(message: &impl Serialize) -> Self {
		// Serializing plain data structures to JSON cannot fail.
		Frame::Control(serde_json::to_vec(message).expect("control messages serialize"))
	}

	/// Reads a control frame's JSON as `T`.
	pub fn parse<T: DeserializeOwned>(payload: &[u8]) -> std::io::Result<T> {
		serde_json::from_slice(payload)
			.map_err(|e| invalid(format!("malformed control frame: {e}")))
	}
}

pub async fn write<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Frame) -> std::io::Result<()> {
	let (tag, payload): (u8, &[u8]) = match frame {
		Frame::Data(data) => (TAG_DATA, data),
		Frame::Hash(hash) => (TAG_HASH, hash),
		Frame::End => (TAG_END, &[]),
		Frame::Control(json) => (TAG_CONTROL, json),
		Frame::Error(message) => (TAG_ERROR, message.as_bytes()),
	};
	if payload.len() > MAX_PAYLOAD {
		return Err(invalid(format!(
			"a {}-byte frame is over the limit",
			payload.len()
		)));
	}

	let mut header = [0u8; 5];
	header[0] = tag;
	header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
	writer.write_all(&header).await?;
	writer.write_all(payload).await?;
	writer.flush().await
}

/// Reads one frame. EOF before a frame has started is `UnexpectedEof`, the
/// same as EOF halfway through one: either way the other side went away
/// without saying it was finished.
pub async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Frame> {
	let mut header = [0u8; 5];
	reader.read_exact(&mut header).await?;

	let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
	if len > MAX_PAYLOAD {
		return Err(invalid(format!("a {len}-byte frame is over the limit")));
	}
	let mut payload = vec![0u8; len];
	reader.read_exact(&mut payload).await?;

	match header[0] {
		TAG_DATA => Ok(Frame::Data(payload)),
		TAG_HASH => {
			let hash: [u8; 32] = payload
				.try_into()
				.map_err(|_| invalid("a hash frame must carry 32 bytes".to_string()))?;
			Ok(Frame::Hash(hash))
		}
		TAG_END if len == 0 => Ok(Frame::End),
		TAG_END => Err(invalid("an end frame carries nothing".to_string())),
		TAG_CONTROL => Ok(Frame::Control(payload)),
		TAG_ERROR => Ok(Frame::Error(String::from_utf8_lossy(&payload).into_owned())),
		other => Err(invalid(format!("unknown frame tag {other:#04x}"))),
	}
}

/// How many frames a [`FrameReader`] reads ahead of whoever is consuming them:
/// at most a few chunks, so a slow consumer stalls the producer rather than
/// making anything in between hold the file in memory.
const READ_AHEAD: usize = 4;

/// Frames from one side of a connection, read by a task of their own.
///
/// [`read`] is not safe to race in a `select!`: losing the race halfway through
/// a frame drops the bytes already consumed and desynchronizes the stream. A
/// transfer has to watch both of its sides at once — the CLI can die while the
/// daemon waits on the network, and the reverse — so each side is read here
/// instead, and [`FrameReader::next`] is a channel receive, which can be raced
/// freely.
pub struct FrameReader {
	frames: tokio::sync::mpsc::Receiver<std::io::Result<Frame>>,
	task: tokio::task::JoinHandle<()>,
}

impl FrameReader {
	pub fn spawn<R: AsyncRead + Unpin + Send + 'static>(mut reader: R) -> Self {
		let (tx, frames) = tokio::sync::mpsc::channel(READ_AHEAD);
		let task = tokio::spawn(async move {
			loop {
				let frame = read(&mut reader).await;
				let failed = frame.is_err();
				if tx.send(frame).await.is_err() || failed {
					return;
				}
			}
		});
		Self { frames, task }
	}

	/// The next frame. Once the other side has gone, every call is an error.
	pub async fn next(&mut self) -> std::io::Result<Frame> {
		match self.frames.recv().await {
			Some(frame) => frame,
			None => Err(std::io::ErrorKind::UnexpectedEof.into()),
		}
	}
}

impl Drop for FrameReader {
	/// A reader blocked on a peer that never writes again would otherwise
	/// outlive the transfer it belonged to.
	fn drop(&mut self) {
		self.task.abort();
	}
}

fn invalid(message: String) -> std::io::Error {
	std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
	use super::*;

	async fn round_trip(frame: Frame) -> Frame {
		let mut wire = Vec::new();
		write(&mut wire, &frame).await.unwrap();
		read(&mut wire.as_slice()).await.unwrap()
	}

	#[tokio::test]
	async fn every_frame_survives_the_wire() {
		for frame in [
			Frame::Data(b"hello".to_vec()),
			Frame::Data(Vec::new()),
			Frame::Hash([7u8; 32]),
			Frame::End,
			Frame::Control(br#"{"a":1}"#.to_vec()),
			Frame::Error("disk full".to_string()),
			Frame::Data(vec![0xAB; MAX_PAYLOAD]),
		] {
			assert_eq!(round_trip(frame.clone()).await, frame);
		}
	}

	#[tokio::test]
	async fn frames_follow_one_another_without_separators() {
		let mut wire = Vec::new();
		write(&mut wire, &Frame::Data(b"one".to_vec()))
			.await
			.unwrap();
		write(&mut wire, &Frame::End).await.unwrap();

		let mut reader = wire.as_slice();
		assert_eq!(
			read(&mut reader).await.unwrap(),
			Frame::Data(b"one".to_vec())
		);
		assert_eq!(read(&mut reader).await.unwrap(), Frame::End);
		// Nothing after the end frame: the stream just stops.
		assert_eq!(
			read(&mut reader).await.unwrap_err().kind(),
			std::io::ErrorKind::UnexpectedEof
		);
	}

	#[tokio::test]
	async fn an_oversized_length_is_refused_before_allocating() {
		let mut wire = vec![TAG_DATA];
		wire.extend_from_slice(&u32::MAX.to_be_bytes());
		let err = read(&mut wire.as_slice()).await.unwrap_err();
		assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
	}

	#[tokio::test]
	async fn an_oversized_frame_is_never_written() {
		let mut wire = Vec::new();
		assert!(write(&mut wire, &Frame::Data(vec![0; MAX_PAYLOAD + 1]))
			.await
			.is_err());
		assert!(wire.is_empty(), "nothing half-written");
	}

	#[tokio::test]
	async fn a_truncated_frame_is_eof_not_a_short_frame() {
		let mut wire = Vec::new();
		write(&mut wire, &Frame::Data(b"abcdef".to_vec()))
			.await
			.unwrap();
		wire.truncate(wire.len() - 2);
		assert_eq!(
			read(&mut wire.as_slice()).await.unwrap_err().kind(),
			std::io::ErrorKind::UnexpectedEof
		);
	}

	#[tokio::test]
	async fn malformed_frames_are_refused() {
		// A hash of the wrong length, an end frame with a payload, an unknown tag.
		for wire in [
			[&[TAG_HASH][..], &3u32.to_be_bytes(), b"abc"].concat(),
			[&[TAG_END][..], &1u32.to_be_bytes(), b"x"].concat(),
			[&b"?"[..], &0u32.to_be_bytes()].concat(),
		] {
			let err = read(&mut wire.as_slice()).await.unwrap_err();
			assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{wire:?}");
		}
	}

	#[tokio::test]
	async fn a_frame_reader_can_be_raced_without_losing_a_frame() {
		// The property `read` lacks: a `select!` that picks the other branch
		// must not eat half a frame.
		let (client, mut server) = tokio::io::duplex(64);
		let mut frames = FrameReader::spawn(client);

		for n in 0..20u8 {
			tokio::select! {
				biased;
				_ = tokio::task::yield_now() => {}
				_ = frames.next() => panic!("nothing has been written yet"),
			}
			write(&mut server, &Frame::Data(vec![n; 100]))
				.await
				.unwrap();
			assert_eq!(frames.next().await.unwrap(), Frame::Data(vec![n; 100]));
		}

		drop(server);
		assert!(frames.next().await.is_err(), "the other side hung up");
		assert!(frames.next().await.is_err(), "and stays hung up");
	}

	#[tokio::test]
	async fn control_frames_carry_typed_messages() {
		#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
		struct Hello {
			n: u32,
		}
		let Frame::Control(json) = round_trip(Frame::control(&Hello { n: 3 })).await else {
			panic!("expected a control frame");
		};
		assert_eq!(Frame::parse::<Hello>(&json).unwrap(), Hello { n: 3 });
		assert!(Frame::parse::<Hello>(b"not json").is_err());
	}
}
