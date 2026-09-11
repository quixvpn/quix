use anyhow::Result;
use interprocess::local_socket::tokio::{prelude::*, Listener, Stream};
use interprocess::local_socket::{
	GenericFilePath, GenericNamespaced, ListenerOptions, ToFsName, ToNsName,
};
use iroh::Endpoint;
use proto::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::state::State;

#[cfg(windows)]
fn build_listener() -> Result<Listener> {
	use interprocess::os::windows::local_socket::ListenerOptionsExt;
	use interprocess::os::windows::security_descriptor::{
		AsSecurityDescriptorMutExt, SecurityDescriptor,
	};
	use std::ptr;

	let raw = proto::socket_name();
	let name = raw.to_ns_name::<GenericNamespaced>()?;

	// Null DACL grants full access to every security principal — same intent
	// as chmod 0o666 on Unix below. Confirmed via interprocess's own test
	// suite (tests/os/windows/local_socket_security_descriptor/null_dacl.rs).
	let mut sd = SecurityDescriptor::new()?;
	unsafe {
		sd.set_dacl(ptr::null_mut(), false)?;
	}

	Ok(ListenerOptions::new()
		.name(name)
		.security_descriptor(sd)
		.create_tokio()?)
}

#[cfg(unix)]
fn build_listener() -> Result<Listener> {
	use std::os::unix::fs::PermissionsExt;

	let raw = proto::socket_name();
	let _ = std::fs::remove_file(&raw);
	let name = raw.clone().to_fs_name::<GenericFilePath>()?;

	let listener = ListenerOptions::new().name(name).create_tokio()?;

	// TODO: tighten this to 0660 + a dedicated "quix" group once quixd
	// runs as a proper system service, instead of world-writable.
	std::fs::set_permissions(&raw, std::fs::Permissions::from_mode(0o666))?;

	Ok(listener)
}

pub async fn serve(endpoint: Endpoint, state: State) -> Result<()> {
	let listener = build_listener()?;

	loop {
		let conn = match listener.accept().await {
			Ok(c) => c,
			Err(e) => {
				eprintln!("incoming ipc connection error: {e}");
				continue;
			}
		};
		let endpoint = endpoint.clone();
		let state = state.clone();
		tokio::spawn(async move {
			if let Err(e) = handle(conn, endpoint, state).await {
				eprintln!("ipc request failed: {e}");
			}
		});
	}
}

async fn handle(conn: Stream, endpoint: Endpoint, state: State) -> Result<()> {
	let mut recver = BufReader::new(&conn);
	let mut sender = &conn;

	let mut line = String::new();
	recver.read_line(&mut line).await?;

	let req: Request = serde_json::from_str(line.trim())?;

	let resp = match req {
		Request::Ping { peer, msg } => {
			match crate::connect::ping(&endpoint, &peer, msg.as_bytes()).await {
				Ok(echo) => Response::Ok {
					echo: String::from_utf8_lossy(&echo).to_string(),
				},
				Err(e) => Response::Error {
					message: e.to_string(),
				},
			}
		}
		Request::Status => Response::Status {
			endpoint_id: endpoint.id().to_string(),
			peer_count: state.peer_count(),
		},
		Request::CreateNetwork { name } => {
			match state.create_network(name, endpoint.id().to_string()).await {
				Ok(()) => Response::Ok {
					echo: "network created".to_string(),
				},
				Err(e) => Response::Error {
					message: e.to_string(),
				},
			}
		}
		Request::Invite => {
			if !state.is_coordinator(&endpoint.id().to_string()).await {
				Response::Error {
					message: "only the coordinator can invite".to_string(),
				}
			} else {
				match state.generate_invite().await {
					Ok(token) => Response::Invite {
						code: format!("{}.{}", endpoint.id(), token),
					},
					Err(e) => Response::Error {
						message: e.to_string(),
					},
				}
			}
		}
		Request::Join { code } => match code.split_once('.') {
			Some((coordinator_id, token)) => {
				match crate::connect::join_network(&endpoint, coordinator_id, token).await {
					Ok(name) => match state
						.set_joined(coordinator_id.to_string(), endpoint.id().to_string(), name.clone())
						.await
					{
						Ok(()) => Response::Joined { network_name: name },
						Err(e) => Response::Error {
							message: e.to_string(),
						},
					},
					Err(e) => Response::Error {
						message: e.to_string(),
					},
				}
			}
			None => Response::Error {
				message: "invalid invite code".to_string(),
			},
		},
	};

	let mut payload = serde_json::to_vec(&resp)?;
	payload.push(b'\n');
	sender.write_all(&payload).await?;

	Ok(())
}