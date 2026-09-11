use anyhow::Result;
use interprocess::local_socket::tokio::{prelude::*, Listener, Stream};
use interprocess::local_socket::{
	GenericFilePath, GenericNamespaced, ListenerOptions, ToFsName, ToNsName,
};
use iroh::Endpoint;
use proto::{Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::state::State;

fn build_listener() -> Result<Listener> {
	let raw = proto::socket_name();

	let name = if cfg!(windows) {
		raw.to_ns_name::<GenericNamespaced>()?
	} else {
		let _ = std::fs::remove_file(&raw); // clean up a stale socket file
		raw.to_fs_name::<GenericFilePath>()?
	};

	Ok(ListenerOptions::new().name(name).create_tokio()?)
}

pub async fn serve(endpoint: Endpoint, state: State) -> Result<()> {
	let listener = build_listener()?;

	loop {
		let stream = listener.accept().await?;
		let endpoint = endpoint.clone();
		let state = state.clone();
		tokio::spawn(async move {
			if let Err(e) = handle(stream, endpoint, state).await {
				eprintln!("ipc request failed: {e}");
			}
		});
	}
}

async fn handle(mut stream: Stream, endpoint: Endpoint, state: State) -> Result<()> {
	let mut buf = Vec::new();
	stream.read_to_end(&mut buf).await?;

	let req: Request = serde_json::from_slice(&buf)?;

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

	let payload = serde_json::to_vec(&resp)?;
	stream.write_all(&payload).await?;

	Ok(())
}