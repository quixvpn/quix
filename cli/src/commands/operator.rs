use anyhow::Result;
use clap::Args;
use proto::{Request, Response};

use super::client::send;

/// Let a local user run mutating commands without sudo
#[derive(Args)]
pub struct SetOperatorArgs {
	/// Username, or a numeric uid
	pub user: String,
}

pub async fn run(args: SetOperatorArgs) -> Result<()> {
	match send(Request::SetOperator { user: args.user }).await? {
		Response::OperatorSet { user, uid } => {
			println!("operator set to {user} (uid {uid})");
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("set-operator failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}
