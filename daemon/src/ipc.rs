use anyhow::Result;
use interprocess::local_socket::tokio::{prelude::*, Listener, Stream};
use interprocess::local_socket::ListenerOptions;
use proto::{PeerStatus, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::state::State;

#[cfg(windows)]
fn build_listener() -> Result<Listener> {
	use interprocess::os::windows::local_socket::ListenerOptionsExt;
	use interprocess::local_socket::{GenericNamespaced, ToNsName};
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
	use interprocess::local_socket::{GenericFilePath, ToFsName};
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

pub async fn serve(state: State) -> Result<()> {
	let listener = build_listener()?;

	loop {
		let conn = match listener.accept().await {
			Ok(c) => c,
			Err(e) => {
				eprintln!("incoming ipc connection error: {e}");
				continue;
			}
		};
		let state = state.clone();
		tokio::spawn(async move {
			if let Err(e) = handle(conn, state).await {
				eprintln!("ipc request failed: {e}");
			}
		});
	}
}

async fn handle(conn: Stream, state: State) -> Result<()> {
	let mut recver = BufReader::new(&conn);
	let mut sender = &conn;

	let mut line = String::new();
	recver.read_line(&mut line).await?;

	let req: Request = serde_json::from_str(line.trim())?;
	let resp = dispatch(req, &state).await;

	let mut payload = serde_json::to_vec(&resp)?;
	payload.push(b'\n');
	sender.write_all(&payload).await?;

	Ok(())
}

async fn dispatch(req: Request, state: &State) -> Response {
	match req {
		Request::Ping { peer } => match crate::connect::probe(state, &peer).await {
			Ok(probe) => Response::Pong {
				virtual_ip: probe.virtual_ip.to_string(),
				rtt_ms: probe.rtt.map(|rtt| rtt.as_secs_f64() * 1000.0),
			},
			Err(e) => error(e),
		},

		Request::Status => {
			let peers = state
				.peers()
				.snapshot()
				.await
				.into_iter()
				.map(|(id, ip, linked)| PeerStatus {
					id: id.to_string(),
					virtual_ip: ip.to_string(),
					linked,
				})
				.collect();

			Response::Status {
				endpoint_id: state.own_id().to_string(),
				virtual_ip: state.virtual_ip().to_string(),
				network: state.network_name().await,
				coordinator: state.is_coordinator().await,
				peers,
			}
		}

		Request::CreateNetwork { name } => match state.create_network(name).await {
			Ok(()) => Response::Ok {
				echo: "network created".to_string(),
			},
			Err(e) => error(e),
		},

		Request::Invite => {
			if !state.is_coordinator().await {
				return Response::Error {
					message: "only the coordinator can invite".to_string(),
				};
			}
			match state.generate_invite().await {
				Ok(code) => Response::Invite { code },
				Err(e) => error(e),
			}
		}

		Request::Join { code } => match crate::connect::join_network(state.endpoint(), &code).await {
			Ok(admission) => {
				let name = admission.network_name.clone();
				match state
					.set_joined(
						admission.coordinator_id.to_string(),
						admission.network_name,
						admission.members,
					)
					.await
				{
					Ok(()) => Response::Joined { network_name: name },
					Err(e) => error(e),
				}
			}
			Err(e) => error(e),
		},
	}
}

/// Renders the whole context chain ("{:#}"), not just the outermost layer —
/// a bare `connect` tells you nothing about why it failed.
fn error(e: anyhow::Error) -> Response {
	Response::Error {
		message: format!("{e:#}"),
	}
}
