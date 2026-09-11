use anyhow::Result;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use serde::{Deserialize, Serialize};

use crate::state::State;

pub const ADMIN_ALPN: &[u8] = b"quix-admin/0";

#[derive(Debug, Serialize, Deserialize)]
struct JoinRequest {
	token: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct JoinResponse {
	ok: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	network_name: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AdminHandler {
	pub state: State,
}

impl ProtocolHandler for AdminHandler {
	async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
		let requester = connection.remote_id().to_string();
		let (mut send, mut recv) = connection.accept_bi().await?;

		let data = recv.read_to_end(1024).await.map_err(std::io::Error::other)?;
		let req: JoinRequest =
			serde_json::from_slice(&data).map_err(std::io::Error::other)?;

		let resp = match self.state.redeem_invite(&req.token, requester.clone()).await {
			Ok(true) => {
				println!("admitted new member: {requester}");
				JoinResponse {
					ok: true,
					network_name: self.state.network_name().await,
					error: None,
				}
			}
			Ok(false) => JoinResponse {
				ok: false,
				network_name: None,
				error: Some("invalid or already used invite".to_string()),
			},
			Err(e) => JoinResponse {
				ok: false,
				network_name: None,
				error: Some(e.to_string()),
			},
		};

		let payload = serde_json::to_vec(&resp).map_err(std::io::Error::other)?;
		send.write_all(&payload).await.map_err(std::io::Error::other)?;
		send.finish()?;

		connection.closed().await;
		Ok(())
	}
}