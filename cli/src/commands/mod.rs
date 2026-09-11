mod client;
mod create;
mod invite;
mod join;
mod leave;
mod operator;
mod ping;
mod status;
mod update;

use clap::{Parser, Subcommand};

/// Rendered with the leading `v` so it matches the release tag it was built
/// from — `v0.1.0` here is the `v0.1.0` tag, with nothing to translate.
pub const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

#[derive(Parser)]
#[command(name = "quix", version = VERSION, about = "P2P mesh VPN over QUIC")]
pub struct Cli {
	#[command(subcommand)]
	pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
	/// Create a new network and become its coordinator
	Create(create::CreateArgs),
	/// Generate a one-time invite code (coordinator only)
	Invite(invite::InviteArgs),
	/// Join a network using an invite code
	Join(join::JoinArgs),
	/// Leave the current network
	Leave(leave::LeaveArgs),
	/// Send a test message to a peer
	Ping(ping::PingArgs),
	/// Show the daemon's status
	Status(status::StatusArgs),
	/// Let a local user run mutating commands without sudo (root only)
	SetOperator(operator::SetOperatorArgs),
	/// Download and install the latest release
	Update(update::UpdateArgs),
	/// Show the installed version
	Version,
}

pub async fn run() -> anyhow::Result<()> {
	let cli = Cli::parse();

	match cli.command {
		Command::Create(args) => create::run(args).await,
		Command::Invite(args) => invite::run(args).await,
		Command::Join(args) => join::run(args).await,
		Command::Leave(args) => leave::run(args).await,
		Command::Ping(args) => ping::run(args).await,
		Command::Status(args) => status::run(args).await,
		Command::SetOperator(args) => operator::run(args).await,
		Command::Update(args) => update::run(args).await,
		Command::Version => {
			println!("quix {VERSION}");
			Ok(())
		}
	}
}