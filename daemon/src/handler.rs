use crate::state::State;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};

#[derive(Debug, Clone)]
pub struct DataHandler {
	pub state: State,
}

impl ProtocolHandler for DataHandler {
	async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
		let peer = connection.remote_id();
		let peer_id = peer.to_string();

		// TODO(phase 2): reject before the handshake completes (via
		// Incoming::refuse) instead of after, to avoid wasting a handshake
		// on unauthorized peers. Requires bypassing the Router abstraction.
		if !self.state.is_member(&peer_id).await {
			println!("rejected data connection from non-member: {peer}");
			connection.close(1u32.into(), b"not a member");
			return Ok(());
		}

		self.state.peer_connected();
		println!("peer connected: {peer}");

		let (send, recv) = connection.accept_bi().await?;
		let tun = self.state.tun.clone();

		let tun_to_peer = tokio::spawn(tun_to_peer(tun.clone(), send));
		let peer_to_tun = tokio::spawn(peer_to_tun(recv, tun));

		tokio::select! {
			_ = tun_to_peer => {}
			_ = peer_to_tun => {}
		}

		connection.closed().await;
		self.state.peer_disconnected();
		Ok(())
	}
}

async fn tun_to_peer(
	tun: std::sync::Arc<tun_rs::AsyncDevice>,
	mut send: iroh::endpoint::SendStream,
) {
	let mut buf = vec![0u8; 1500];
	loop {
		let len = match tun.recv(&mut buf).await {
			Ok(len) => len,
			Err(e) => {
				eprintln!("tun read failed: {e}");
				return;
			}
		};
		if let Err(e) = send.write_all(&buf[..len]).await {
			eprintln!("write to peer failed: {e}");
			return;
		}
	}
}

async fn peer_to_tun(
	mut recv: iroh::endpoint::RecvStream,
	tun: std::sync::Arc<tun_rs::AsyncDevice>,
) {
	let mut buf = vec![0u8; 1500];
	loop {
		let len = match recv.read(&mut buf).await {
			Ok(Some(len)) => len,
			Ok(None) => return,
			Err(e) => {
				eprintln!("read from peer failed: {e}");
				return;
			}
		};
		if let Err(e) = tun.send(&buf[..len]).await {
			eprintln!("tun write failed: {e}");
			return;
		}
	}
}