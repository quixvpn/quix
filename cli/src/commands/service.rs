use anyhow::Result;
use clap::{Args, Subcommand};

use crate::service_manager as manager;

/// Control the background daemon
#[derive(Args)]
pub struct ServiceArgs {
	#[command(subcommand)]
	pub action: Action,
}

#[derive(Subcommand)]
pub enum Action {
	/// Start the daemon now, if it isn't already
	Start,
	/// Stop the daemon now
	Stop,
	/// Stop and start it again
	Restart,
	/// Start the daemon automatically at boot
	Enable,
	/// Stop starting the daemon at boot (leaves it running now)
	Disable,
	/// Whether it's running, and whether it starts at boot
	Status,
}

pub async fn run(args: ServiceArgs) -> Result<()> {
	let state = manager::state();

	if !state.installed {
		anyhow::bail!(
			"the {} service is not installed — run the installer first \
			 (see the README), or start the daemon by hand",
			manager::NAME
		);
	}

	match args.action {
		Action::Status => {
			println!(
				"{:<12} {}",
				"running",
				if state.running { "yes" } else { "no" }
			);
			println!(
				"{:<12} {}",
				"at boot",
				if state.enabled { "enabled" } else { "disabled" }
			);
			Ok(())
		}

		Action::Start => {
			if state.running {
				println!("already running");
				return Ok(());
			}
			manager::start()?;
			settle()
		}

		Action::Stop => {
			if !state.running {
				println!("already stopped");
				return Ok(());
			}
			manager::stop()?;
			println!("stopped");
			Ok(())
		}

		Action::Restart => {
			manager::restart()?;
			settle()
		}

		Action::Enable => {
			manager::enable()?;
			println!("will start at boot");
			if !state.running {
				println!("note: not running now — `quix service start` to start it");
			}
			Ok(())
		}

		Action::Disable => {
			manager::disable()?;
			println!("will not start at boot");
			if state.running {
				println!("note: still running — `quix service stop` to stop it now");
			}
			Ok(())
		}
	}
}

/// The service manager reports success as soon as the process exists, which is
/// before the daemon has picked a relay, brought the TUN up and bound its
/// socket. Wait for that, so returning means the next command will work.
fn settle() -> Result<()> {
	if manager::wait_until_ready() {
		println!("running");
		return Ok(());
	}

	anyhow::bail!(
		"started, but the daemon did not become answerable — check the log\n\
		 (Linux: journalctl -u {}; Windows: Event Viewer, Windows Logs, Application)",
		manager::NAME
	)
}
