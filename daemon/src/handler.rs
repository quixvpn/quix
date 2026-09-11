use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};

#[derive(Debug, Clone)]
pub struct Echo;

impl ProtocolHandler for Echo {
	async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
		let (mut send, mut recv) = connection.accept_bi().await?;

		let data = recv
            .read_to_end(1024)
            .await
            .map_err(|e| AcceptError::from_err(e))?;
		println!("received: {}", String::from_utf8_lossy(&data));

		send.write_all(&data)
			.await
			.map_err(std::io::Error::other)?;
		send.finish()?;

		connection.closed().await;
		Ok(())
	}
}