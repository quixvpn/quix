use anyhow::{Context, Result};
use interprocess::local_socket::tokio::{prelude::*, Listener, Stream};
use interprocess::local_socket::ListenerOptions;
use proto::{PeerStatus, Request, Response, Traffic};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::authz::{self, Caller};
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
	// /run is a tmpfs, so our directory is gone after every boot. systemd's
	// RuntimeDirectory= also makes it, but the daemon has to work when run
	// straight from a build tree too.
	if let Some(parent) = std::path::Path::new(&raw).parent() {
		std::fs::create_dir_all(parent)?;
	}
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

	// The kernel tells us who is connected; the client never gets to claim it.
	let caller = Caller::of(&conn);
	let resp = match authz::check(&req, &caller, state.operator_uid().await) {
		Ok(()) => dispatch(req, &state).await,
		Err(message) => {
			eprintln!("refused {req:?} from {}", caller.describe());
			Response::Error { message }
		}
	};

	let mut payload = serde_json::to_vec(&resp)?;
	payload.push(b'\n');
	sender.write_all(&payload).await?;

	Ok(())
}

async fn dispatch(req: Request, state: &State) -> Response {
	match req {
		Request::Ping { peer } => match crate::connect::probe(state, &peer).await {
			Ok(probe) => Response::Pong {
				v6: probe.v6.to_string(),
				v4: probe.v4.to_string(),
				rtt_ms: probe.rtt.map(|rtt| rtt.as_secs_f64() * 1000.0),
			},
			Err(e) => error(e),
		},

		Request::Status => {
			let hostnames = state.hostnames().await;
			let own_hostname = state.hostname().await;
			let zone = state
				.network_name()
				.await
				.map(|n| format!("{}.{}", crate::names::network_label(&n), crate::dns::ZONE))
				.unwrap_or_else(|| crate::dns::ZONE.to_string());
			let peers = state
				.peers()
				.snapshot()
				.await
				.into_iter()
				.map(|row| {
					let id = row.id.to_string();
					let hostname = hostnames.get(&id).cloned();
					PeerStatus {
						name: crate::names::display(&id, hostname.as_deref()),
						named: hostname.is_some(),
						id,
						v6: row.v6.to_string(),
						v4: row.v4.to_string(),
						linked: row.linked,
						datagram_max: row.datagram_max.map(|max| max as u32),
					}
				})
				.collect();

			let s = state.stats().snapshot();
			let (v4, v6) = state.virtual_addrs();
			Response::Status {
				traffic: Traffic {
					tun_rx: s.tun_rx,
					tun_tx: s.tun_tx,
					mesh_tx: s.mesh_tx,
					mesh_rx: s.mesh_rx,
					no_route: s.no_route,
					no_link: s.no_link,
					oversize: s.oversize,
					send_err: s.send_err,
					tun_tx_err: s.tun_tx_err,
				},
				endpoint_id: state.own_id().to_string(),
				name: crate::names::display(&state.own_id().to_string(), own_hostname.as_deref()),
				named: own_hostname.is_some(),
				v6: v6.to_string(),
				v4: v4.to_string(),
				network: state.network_name().await,
				coordinator: state.is_coordinator().await,
				peers,
				zone: zone.clone(),
				conflicts: state.conflicts().await,
			}
		}

		Request::CreateNetwork { name, hostname } => match create_network(state, name, hostname).await {
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

		Request::SetHostname { hostname, force } => match set_hostname(state, &hostname, force).await {
			Ok(assigned) => Response::HostnameSet {
				hostname: assigned,
				requested: hostname,
			},
			Err(e) => error(e),
		},

		Request::SetOperator { user } => match authz::resolve_user(&user) {
			Ok(uid) => match state.set_operator(user.clone(), uid).await {
				Ok(()) => Response::OperatorSet { user, uid },
				Err(e) => error(e),
			},
			Err(message) => Response::Error { message },
		},

		Request::Leave => {
			// Tell the coordinator while we still know who it is. Best-effort:
			// if they're offline we still leave locally, and their roster
			// catches up when they next see us refuse a link.
			let coordinator = state.coordinator_id().await;
			let own_id = state.own_id().to_string();
			let mut coordinator_notified = false;

			if let Some(id) = coordinator.filter(|id| *id != own_id) {
				match id.parse() {
					Ok(id) => match crate::connect::notify_leave(state.endpoint(), id).await {
						Ok(()) => coordinator_notified = true,
						Err(e) => eprintln!("telling the coordinator we left failed: {e:#}"),
					},
					Err(e) => eprintln!("stored coordinator id is unusable: {e}"),
				}
			}

			match state.leave().await {
				Ok(network_name) => Response::Left {
					network_name,
					coordinator_notified,
				},
				Err(e) => error(e),
			}
		}

		Request::Join { code, hostname } => match crate::connect::join_network(state.endpoint(), &code, hostname).await {
			Ok(admission) => {
				let name = admission.network_name.clone();
				// May differ from what was requested: the coordinator resolves
				// collisions, so tell the user which name they actually got.
				let assigned = admission.hostname.clone();
				match state
					.set_joined(
						admission.coordinator_id.to_string(),
						admission.network_name,
						admission.members,
					)
					.await
				{
					Ok(()) => Response::Joined {
						network_name: name,
						hostname: assigned,
					},
					Err(e) => error(e),
				}
			}
			Err(e) => error(e),
		},
	}
}

/// Validates the hostname before it reaches the roster, so an unusable label is
/// refused at the point of setting rather than at resolution time.
async fn create_network(
	state: &State,
	name: String,
	hostname: Option<String>,
) -> anyhow::Result<()> {
	let name = crate::names::validate_network(&name).map_err(|e| anyhow::anyhow!(e))?;
	let hostname = match hostname {
		Some(requested) => Some(
			crate::names::validate(&requested, &state.own_id().to_string())
				.map_err(|e| anyhow::anyhow!(e))?,
		),
		None => None,
	};
	state.create_network(name, hostname).await
}

/// Sets this node's hostname.
///
/// The coordinator is the source of truth for collisions, so a member asks it
/// and adopts whatever comes back. If it cannot be reached the name is still
/// recorded locally — the alternative is being unable to name a machine while
/// the coordinator is down — and the next roster push reconciles it.
async fn set_hostname(state: &State, requested: &str, force: bool) -> anyhow::Result<String> {
	let own_id = state.own_id().to_string();
	let wanted = crate::names::validate(requested, &own_id).map_err(|e| anyhow::anyhow!(e))?;

	if state.is_coordinator().await {
		let assigned = state
			.claim_hostname(&own_id, &wanted, force)
			.await?
			.map_err(|e| anyhow::anyhow!(e))?;

		// Renaming ourselves changes the roster exactly as renaming a member
		// does, so it has to be pushed the same way — otherwise everyone else
		// keeps calling us by our fallback.
		crate::admin::broadcast_roster(
			state,
			&own_id,
			state.network_name().await,
			state.roster().await,
		);
		return Ok(assigned);
	}

	let coordinator = state
		.coordinator_id()
		.await
		.context("not a member of any network")?;
	let coordinator: iroh::EndpointId = coordinator.parse().context("stored coordinator id")?;

	match crate::connect::claim_hostname(state.endpoint(), coordinator, wanted.clone()).await {
		Ok(assigned) => {
			state.adopt_hostname(assigned.clone()).await?;
			Ok(assigned)
		}
		Err(e) => {
			eprintln!("claiming {wanted:?} with the coordinator failed: {e:#}");
			state.adopt_hostname(wanted.clone()).await?;
			Ok(wanted)
		}
	}
}

/// Renders the whole context chain ("{:#}"), not just the outermost layer —
/// a bare `connect` tells you nothing about why it failed.
fn error(e: anyhow::Error) -> Response {
	Response::Error {
		message: format!("{e:#}"),
	}
}
