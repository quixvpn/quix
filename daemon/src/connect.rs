use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointId};

use crate::ALPN;

pub async fn ping(endpoint: &Endpoint, peer_id: &str, msg: &[u8]) -> Result<Vec<u8>> {
	let id: EndpointId = peer_id.parse().context("parse peer id")?;

	let conn = endpoint.connect(id, ALPN).await.context("connect")?;

	let (mut send, mut recv) = conn.open_bi().await.context("open stream")?;

	send.write_all(msg).await.context("write")?;
	send.finish().context("finish")?;

	let echo = recv.read_to_end(1024).await.context("read echo")?;

	conn.close(0u32.into(), b"done");

	Ok(echo)
}

pub async fn join_network(
	endpoint: &Endpoint,
	coordinator_id: &str,
	token: &str,
) -> Result<Option<String>> {
	let id: EndpointId = coordinator_id.parse().context("parse coordinator id")?;

	let conn = endpoint
		.connect(id, crate::admin::ADMIN_ALPN)
		.await
		.context("connect")?;

	let (mut send, mut recv) = conn.open_bi().await.context("open stream")?;

	let payload = serde_json::to_vec(&serde_json::json!({ "token": token }))?;
	send.write_all(&payload).await.context("write")?;
	send.finish().context("finish")?;

	let data = recv.read_to_end(1024).await.context("read response")?;
	let resp: serde_json::Value = serde_json::from_slice(&data)?;

	conn.close(0u32.into(), b"done");

	if resp["ok"].as_bool().unwrap_or(false) {
		Ok(resp["network_name"].as_str().map(|s| s.to_string()))
	} else {
		anyhow::bail!(resp["error"].as_str().unwrap_or("join failed").to_string())
	}
}