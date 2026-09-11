use anyhow::Result;
use clap::Args;
use proto::{Request, Response};

use super::client::send;

/// Generate a one-time invite code (coordinator only)
#[derive(Args)]
pub struct InviteArgs;

pub async fn run(_args: InviteArgs) -> Result<()> {
	match send(Request::Invite).await? {
		Response::Invite { code } => {
			println!("invite code: {code}");
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("invite failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}