use anyhow::Result;
use interprocess::local_socket::tokio::{prelude::*, Listener, Stream};
use interprocess::local_socket::{
	GenericFilePath, GenericNamespaced, ListenerOptions, ToFsName, ToNsName,
};
use iroh::Endpoint;
use proto::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::state::State;

fn build_listener() -> Result<Listener> {
	let raw = proto::socket_name();

	let name = if cfg!(windows) {
		raw.clone().to_ns_name::<GenericNamespaced>()?
	} else {
		let _ = std::fs::remove_file(&raw);
		raw.clone().to_fs_name::<GenericFilePath>()?
	};

	let listener = ListenerOptions::new().name(name).create_tokio()?;

	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		std::fs::set_permissions(&raw, std::fs::Permissions::from_mode(0o666))?;
	}

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
	};

	let mut payload = serde_json::to_vec(&resp)?;
	payload.push(b'\n');
	sender.write_all(&payload).await?;

	Ok(())
}