// daemon/src/handler.rs
use crate::state::State;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};

#[derive(Debug, Clone)]
pub struct Echo {
	pub state: State,
}

impl ProtocolHandler for Echo {
	async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
		self.state.peer_connected();
		let peer = connection.remote_id();
		println!("peer connected: {peer}");

		let (mut send, mut recv) = connection.accept_bi().await?;
		let data = recv.read_to_end(1024).await.map_err(std::io::Error::other)?;

		send.write_all(&data).await.map_err(std::io::Error::other)?;
		send.finish()?;

		connection.closed().await;
		self.state.peer_disconnected();
		Ok(())
	}
}