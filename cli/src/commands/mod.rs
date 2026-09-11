mod client;
mod ping;
mod status;

use clap::{Parser, Subcommand};

// this is the root command file

#[derive(Parser)]
#[command(name = "quix", about = "P2P mesh VPN over QUIC")]
pub struct Cli {
	#[command(subcommand)]
	pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Send a test message to a peer
	Ping(ping::PingArgs),
    /// Show the daemon's status
	Status(status::StatusArgs),
}

pub async fn run() -> anyhow::Result<()> {
	let cli = Cli::parse();

	match cli.command {
		Command::Ping(args) => ping::run(args).await,
		Command::Status(args) => status::run(args).await,
	}
}