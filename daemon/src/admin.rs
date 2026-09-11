use anyhow::Result;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use serde::{Deserialize, Serialize};

use crate::state::State;

pub const ADMIN_ALPN: &[u8] = b"quix-admin/0";

/// One request per bidirectional stream, read to end.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AdminRequest {
	/// Joiner → coordinator: redeem an invite token.
	Join { token_hex: String },
	/// Member → coordinator: remove me from the roster.
	Leave,
	/// Coordinator → member: the roster has changed, here is the new one.
	Roster {
		network_name: Option<String>,
		members: Vec<String>,
	},
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AdminResponse {
	Joined {
		network_name: Option<String>,
		members: Vec<String>,
	},
	Ack,
	Error {
		message: String,
	},
}

#[derive(Debug, Clone)]
pub struct AdminHandler {
	pub state: State,
}

impl ProtocolHandler for AdminHandler {
	async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
		let requester = connection.remote_id().to_string();
		let (mut send, mut recv) = connection.accept_bi().await?;

		let data = recv.read_to_end(64 * 1024).await.map_err(std::io::Error::other)?;
		let resp = match serde_json::from_slice(&data) {
			Ok(req) => self.dispatch(req, requester).await,
			Err(e) => AdminResponse::Error {
				message: format!("malformed admin request: {e}"),
			},
		};

		let payload = serde_json::to_vec(&resp).map_err(std::io::Error::other)?;
		send.write_all(&payload).await.map_err(std::io::Error::other)?;
		send.finish()?;

		connection.closed().await;
		Ok(())
	}
}

impl AdminHandler {
	async fn dispatch(&self, req: AdminRequest, requester: String) -> AdminResponse {
		match req {
			AdminRequest::Join { token_hex } => self.join(token_hex, requester).await,
			AdminRequest::Leave => self.leave(requester).await,
			AdminRequest::Roster {
				network_name,
				members,
			} => self.roster(network_name, members, requester).await,
		}
	}

	async fn join(&self, token_hex: String, requester: String) -> AdminResponse {
		let token = match decode_token(&token_hex) {
			Ok(token) => token,
			Err(e) => return AdminResponse::Error { message: e },
		};

		match self.state.redeem_invite(&token, requester.clone()).await {
			Ok(Some(roster)) => {
				println!("admitted new member: {requester}");
				let network_name = self.state.network_name().await;

				// Tell everyone already in the network about the new member, so
				// the mesh is fully connected rather than a star through us.
				self.broadcast_roster(&requester, network_name.clone(), roster.clone());

				AdminResponse::Joined {
					network_name,
					members: roster,
				}
			}
			Ok(None) => AdminResponse::Error {
				message: "invalid or already used invite".to_string(),
			},
			Err(e) => AdminResponse::Error {
				message: e.to_string(),
			},
		}
	}

	async fn leave(&self, requester: String) -> AdminResponse {
		if !self.state.is_coordinator().await {
			return AdminResponse::Error {
				message: "only the coordinator maintains the roster".to_string(),
			};
		}

		match self.state.remove_member(&requester).await {
			// Already gone is the state they asked for, so not an error.
			Ok(false) => AdminResponse::Ack,
			Ok(true) => {
				println!("member left: {requester}");
				let network_name = self.state.network_name().await;
				let roster = self.state.roster().await;
				self.broadcast_roster(&requester, network_name, roster);
				AdminResponse::Ack
			}
			Err(e) => AdminResponse::Error {
				message: e.to_string(),
			},
		}
	}

	async fn roster(
		&self,
		network_name: Option<String>,
		members: Vec<String>,
		requester: String,
	) -> AdminResponse {
		// Only the coordinator gets to rewrite our roster; otherwise any peer
		// that can reach us could add itself to the network.
		if self.state.coordinator_id().await.as_deref() != Some(requester.as_str()) {
			return AdminResponse::Error {
				message: "only the coordinator can push a roster".to_string(),
			};
		}

		match self.state.set_roster(network_name, members).await {
			Ok(()) => AdminResponse::Ack,
			Err(e) => AdminResponse::Error {
				message: e.to_string(),
			},
		}
	}

	/// Fire-and-forget roster push to every member except us and the joiner
	/// (the joiner already got the roster in its join response).
	fn broadcast_roster(&self, joiner: &str, network_name: Option<String>, roster: Vec<String>) {
		let own_id = self.state.own_id().to_string();
		let targets: Vec<String> = roster
			.iter()
			.filter(|id| **id != own_id && id.as_str() != joiner)
			.cloned()
			.collect();

		for target in targets {
			let state = self.state.clone();
			let network_name = network_name.clone();
			let roster = roster.clone();
			tokio::spawn(async move {
				let Ok(id) = target.parse() else { return };
				if let Err(e) =
					crate::connect::push_roster(state.endpoint(), id, network_name, roster).await
				{
					eprintln!("roster push to {target} failed: {e}");
				}
			});
		}
	}
}

fn decode_token(token_hex: &str) -> Result<[u8; 16], String> {
	let bytes = hex::decode(token_hex).map_err(|e| format!("invalid token: {e}"))?;
	bytes
		.try_into()
		.map_err(|_| "invalid token length".to_string())
}
