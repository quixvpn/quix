use crate::mesh;
use crate::state::State;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};

/// Accepts inbound data-plane connections. The link itself is run by
/// [`mesh::serve_link`], which is the same code path a dialed link takes.
///
/// TODO(phase 2): reject before the handshake completes (via Incoming::refuse)
/// instead of after, to avoid wasting a handshake on unauthorized peers.
/// Requires bypassing the Router abstraction.
#[derive(Debug, Clone)]
pub struct DataHandler {
	pub state: State,
}

impl ProtocolHandler for DataHandler {
	async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
		mesh::serve_link(self.state.clone(), connection).await;
		Ok(())
	}
}
