use anyhow::Result;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use serde::{Deserialize, Serialize};

use crate::membership::Member;
use crate::state::State;

pub const ADMIN_ALPN: &[u8] = b"quix-admin/0";

/// One request per bidirectional stream, read to end.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AdminRequest {
	/// Joiner → coordinator: redeem an invite token, optionally asking for a
	/// hostname. The name is a request, not an assertion — the coordinator
	/// resolves collisions and answers with what was actually assigned.
	Join {
		token_hex: String,
		#[serde(default)]
		hostname: Option<String>,
	},
	/// Member → coordinator: remove me from the roster.
	Leave,
	/// Member → coordinator: claim this hostname for me.
	///
	/// Authenticated by the connection itself — iroh's handshake proves the
	/// peer's key — so the coordinator knows which member is asking and will
	/// only ever bind the name to that key.
	SetHostname { hostname: String },
	/// Coordinator → member: the roster has changed, here is the new one.
	Roster {
		network_name: Option<String>,
		members: Vec<Member>,
	},
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AdminResponse {
	Joined {
		network_name: Option<String>,
		members: Vec<Member>,
		/// The hostname the coordinator actually assigned, which may carry a
		/// numeric suffix if the requested one was taken.
		#[serde(default)]
		hostname: Option<String>,
	},
	/// The hostname a claim was granted, deduplicated if it had to be.
	HostnameSet { hostname: String },
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
			AdminRequest::Join {
				token_hex,
				hostname,
			} => self.join(token_hex, hostname, requester).await,
			AdminRequest::Leave => self.leave(requester).await,
			AdminRequest::SetHostname { hostname } => self.set_hostname(hostname, requester).await,
			AdminRequest::Roster {
				network_name,
				members,
			} => self.roster(network_name, members, requester).await,
		}
	}

	async fn join(
		&self,
		token_hex: String,
		hostname: Option<String>,
		requester: String,
	) -> AdminResponse {
		// Defence in depth. Only a coordinator has invites to redeem, so this
		// should be unreachable — but it is the check that was missing when a
		// token minted under one network could still be spent after the node
		// had moved to another.
		if !self.state.is_coordinator().await {
			return AdminResponse::Error {
				message: "this node does not admit members to a network".to_string(),
			};
		}

		let token = match decode_token(&token_hex) {
			Ok(token) => token,
			Err(e) => return AdminResponse::Error { message: e },
		};

		match self
			.state
			.redeem_invite(&token, requester.clone(), hostname)
			.await
		{
			Ok(Some((roster, hostname))) => {
				crate::info!("admitted new member: {requester}");
				let network_name = self.state.network_name().await;

				// Tell everyone already in the network about the new member, so
				// the mesh is fully connected rather than a star through us.
				self.broadcast_roster(&requester, network_name.clone(), roster.clone());

				AdminResponse::Joined {
					network_name,
					members: roster,
					hostname,
				}
			}
			// One answer for every way a code can fail — unknown, spent, past
			// its window, or minted for another network. Which of those it was
			// is not a joiner's business.
			Ok(None) => AdminResponse::Error {
				message: "invalid, expired, or already used invite".to_string(),
			},
			Err(e) => AdminResponse::Error {
				message: e.to_string(),
			},
		}
	}

	/// A member claiming a hostname. The claim is bound to the connection's
	/// authenticated key, so a member can only ever name itself.
	async fn set_hostname(&self, hostname: String, requester: String) -> AdminResponse {
		if !self.state.is_coordinator().await {
			return AdminResponse::Error {
				message: "only the coordinator assigns hostnames".to_string(),
			};
		}
		if !self.state.is_member(&requester).await {
			return AdminResponse::Error {
				message: "not a member of this network".to_string(),
			};
		}

		match self.state.claim_hostname(&requester, &hostname, false).await {
			Ok(Ok(assigned)) => {
				crate::info!("{requester} is now {assigned}");
				let network_name = self.state.network_name().await;
				let roster = self.state.roster().await;
				self.broadcast_roster(&requester, network_name, roster);
				AdminResponse::HostnameSet { hostname: assigned }
			}
			Ok(Err(message)) => AdminResponse::Error { message },
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
				crate::info!("member left: {requester}");
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
		members: Vec<Member>,
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

	fn broadcast_roster(&self, subject: &str, network_name: Option<String>, roster: Vec<Member>) {
		broadcast_roster(&self.state, subject, network_name, roster)
	}
}

/// Fire-and-forget roster push to every member except us and the subject of the
/// change (who already learned the outcome from the reply that triggered this).
///
/// Free-standing because the coordinator also changes the roster from the IPC
/// side — renaming itself — and that has to reach everyone too.
pub fn broadcast_roster(
	state: &State,
	subject: &str,
	network_name: Option<String>,
	roster: Vec<Member>,
) {
	let own_id = state.own_id().to_string();
	let targets: Vec<String> = roster
		.iter()
		.map(|m| m.id.clone())
		.filter(|id| *id != own_id && id != subject)
		.collect();

	for target in targets {
		let state = state.clone();
		let network_name = network_name.clone();
		let roster = roster.clone();
		tokio::spawn(async move {
			let Ok(id) = target.parse() else { return };
			if let Err(e) =
				crate::connect::push_roster(state.endpoint(), id, network_name, roster).await
			{
				crate::warn!("roster push to {target} failed: {e}");
			}
		});
	}
}

fn decode_token(token_hex: &str) -> Result<[u8; 16], String> {
	let bytes = hex::decode(token_hex).map_err(|e| format!("invalid token: {e}"))?;
	bytes
		.try_into()
		.map_err(|_| "invalid token length".to_string())
}
