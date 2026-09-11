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