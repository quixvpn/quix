use anyhow::Result;
use iroh::Endpoint;
use proto::{Request, Response};
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

pub async fn serve(endpoint: Endpoint) -> Result<()> {
	let path = proto::socket_path();
	let _ = std::fs::remove_file(&path); // clean up a stale socket from a previous run

	let listener = UnixListener::bind(&path)?;
	std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

	loop {
		let (stream, _) = listener.accept().await?;
		let endpoint = endpoint.clone();
		tokio::spawn(async move {
			if let Err(e) = handle(stream, endpoint).await {
				eprintln!("ipc request failed: {e}");
			}
		});
	}
}

async fn handle(mut stream: UnixStream, endpoint: Endpoint) -> Result<()> {
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
	};

	let payload = serde_json::to_vec(&resp)?;
	stream.write_all(&payload).await?;

	Ok(())
}