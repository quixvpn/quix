mod client;
mod create;
mod hostname;
mod invite;
mod join;
mod leave;
mod operator;
mod ping;
mod service;
mod status;
mod update;

use clap::{Parser, Subcommand};

/// Rendered with the leading `v` so it matches the release tag it was built
/// from, with nothing to translate between the two.
pub const VERSION: &str = proto::VERSION_TAG;

#[derive(Parser)]
#[command(
	name = "quix",
	about = "P2P mesh VPN over QUIC.",
	arg_required_else_help = true,
	before_help = r#"
    
 ██████╗ ██╗   ██╗██╗██╗  ██╗
██╔═══██╗██║   ██║██║╚██╗██╔╝
██║   ██║██║   ██║██║ ╚███╔╝ 
██║▄▄ ██║██║   ██║██║ ██╔██╗ 
╚██████╔╝╚██████╔╝██║██╔╝ ██╗
 ╚══▀▀═╝  ╚═════╝ ╚═╝╚═╝  ╚═╝
"#
)]
pub struct Cli {
	/// Where an elevated relaunch writes its output, so the process that asked
	/// for elevation can print it. Hidden: it is plumbing between two copies of
	/// this program, and its presence also marks a run as already elevated so
	/// it never asks a second time.
	#[arg(long, hide = true, value_name = "PATH")]
	pub elevated_output: Option<std::path::PathBuf>,

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
	/// Set this machine's hostname on the mesh
	Hostname(hostname::HostnameArgs),
	/// Send a test message to a peer
	Ping(ping::PingArgs),
	/// Show the daemon's status
	Status(status::StatusArgs),
	/// Let a local user run mutating commands without sudo (root only)
	SetOperator(operator::SetOperatorArgs),
	/// Control the background daemon
	Service(service::ServiceArgs),
	/// Download and install the latest release
	Update(update::UpdateArgs),
	/// Show the installed version
	Version,
}

/// Commands that cannot even start without Administrator on Windows.
///
/// Only the ones the *operating system* refuses: the service actions go through
/// the Service Control Manager and `update` rewrites `Program Files`, and
/// neither gives a useful answer to an unelevated process, so there is nothing
/// to do but ask first.
///
/// The membership commands are deliberately absent. They are gated by the
/// daemon's own authorization, which the installing user already satisfies, so
/// they are attempted first and only elevate if the daemon actually refuses —
/// see `client::send`. Asking up front would put a UAC dialog in front of the
/// one person who does not need it.
///
/// Everything else must keep working with no prompt at all: reading state is
/// not privileged, and a dialog in front of `quix status` teaches people to
/// click through them.
#[cfg(windows)]
fn needs_elevation(command: &Command) -> bool {
	match command {
		// Reading the service's state is not privileged; changing it is.
		Command::Service(args) => !matches!(args.action, service::Action::Status),
		// `--check` only asks GitHub what exists and prints it.
		Command::Update(args) => !args.check,
		// Authorized by the daemon, so these prompt on refusal rather than on
		// principle.
		Command::Create(_)
		| Command::Invite(_)
		| Command::Join(_)
		| Command::Leave(_)
		| Command::Hostname(_) => false,
		// Not supported on Windows at all, so a prompt would buy a UAC dialog
		// and then an error. Rejected up front instead, in `run`.
		Command::SetOperator(_) => false,
		Command::Status(_) | Command::Ping(_) | Command::Version => false,
	}
}

/// Validation clap cannot express, run before anything else happens.
///
/// Arguments that relate to each other — `--expires 30 min` is one option in
/// two words — can only be judged together. Doing it here rather than inside
/// the command keeps a rejection instant: on Windows the commands below this
/// point may ask for elevation first, and a UAC prompt followed by "unknown
/// unit" is a bad trade.
fn check_args(command: &Command) -> anyhow::Result<()> {
	match command {
		Command::Invite(args) => invite::ttl_secs(args).map(|_| ()).map_err(|e| anyhow::anyhow!(e)),
		_ => Ok(()),
	}
}

pub async fn run() -> anyhow::Result<()> {
	let cli = Cli::parse();
	check_args(&cli.command)?;

	#[cfg(windows)]
	{
		if matches!(cli.command, Command::SetOperator(_)) {
			anyhow::bail!(
				"set-operator is not supported on Windows: authorization here is by \
				 Administrator elevation, so run the command elevated instead"
			);
		}

		match &cli.elevated_output {
			// We are the elevated relaunch. Send everything to the file the
			// process that asked for elevation is waiting on.
			Some(path) => crate::elevate::redirect_output(path)?,
			None => {
				if needs_elevation(&cli.command) && !crate::elevate::is_elevated() {
					crate::elevate::relaunch();
				}
			}
		}
	}

	match cli.command {
		Command::Create(args) => create::run(args).await,
		Command::Invite(args) => invite::run(args).await,
		Command::Join(args) => join::run(args).await,
		Command::Leave(args) => leave::run(args).await,
		Command::Hostname(args) => hostname::run(args).await,
		Command::Ping(args) => ping::run(args).await,
		Command::Status(args) => status::run(args).await,
		Command::SetOperator(args) => operator::run(args).await,
		Command::Service(args) => service::run(args).await,
		Command::Update(args) => update::run(args).await,
		Command::Version => {
			println!("quix {VERSION}");
			Ok(())
		}
	}
}

#[cfg(all(test, windows))]
mod tests {
	use super::*;
	use clap::CommandFactory;

	fn elevates(args: &[&str]) -> bool {
		let cli = Cli::try_parse_from(args).expect("should parse");
		needs_elevation(&cli.command)
	}

	#[test]
	fn reading_state_never_prompts() {
		// A UAC dialog for these would train people to click through them.
		assert!(!elevates(&["quix", "status"]));
		assert!(!elevates(&["quix", "status", "-v"]));
		assert!(!elevates(&["quix", "ping", "nas"]));
		assert!(!elevates(&["quix", "version"]));
		assert!(!elevates(&["quix", "service", "status"]));
	}

	#[test]
	fn controlling_the_service_prompts() {
		for action in ["start", "stop", "restart", "enable", "disable"] {
			assert!(elevates(&["quix", "service", action]), "service {action}");
		}
	}

	#[test]
	fn replacing_the_installed_binaries_prompts() {
		assert!(elevates(&["quix", "update"]));
		assert!(elevates(&["quix", "update", "--force"]));
	}

	#[test]
	fn asking_what_is_available_does_not() {
		// `--check` returns before it touches the install directory, so the
		// decision has to follow the parsed arguments, not just the subcommand.
		assert!(!elevates(&["quix", "update", "--check"]));
	}

	#[test]
	fn commands_the_daemon_authorizes_do_not_prompt_up_front() {
		// The installing user is already the operator, so prompting before
		// asking would put a UAC dialog in front of the one person who does not
		// need one. These attempt first and elevate only if the daemon refuses
		// — see `client::send`.
		assert!(!elevates(&["quix", "create", "homelab"]));
		assert!(!elevates(&["quix", "invite"]));
		assert!(!elevates(&["quix", "join", "somecode"]));
		assert!(!elevates(&["quix", "leave"]));
		assert!(!elevates(&["quix", "hostname", "nas"]));
	}

	#[test]
	fn only_what_the_os_itself_refuses_prompts_up_front() {
		// The dividing line: the SCM and Program Files give an unelevated
		// process nothing useful, so there is no point asking them first.
		// Everything else is the daemon's decision, and the daemon can say no
		// cheaply.
		for args in [
			vec!["quix", "service", "restart"],
			vec!["quix", "update"],
		] {
			assert!(elevates(&args), "{args:?}");
		}
		for args in [
			vec!["quix", "create", "homelab"],
			vec!["quix", "status"],
			vec!["quix", "service", "status"],
		] {
			assert!(!elevates(&args), "{args:?}");
		}
	}

	#[test]
	fn set_operator_does_not_prompt_because_elevation_would_not_help() {
		// It is unsupported on Windows outright; `run` rejects it up front, so a
		// prompt would buy a UAC dialog followed by an error.
		assert!(!elevates(&["quix", "set-operator", "someone"]));
	}

	#[test]
	fn the_output_flag_is_read_before_the_subcommand() {
		// Exactly the shape `elevate::relaunch` builds.
		let cli = Cli::try_parse_from(["quix", "--elevated-output", r"C:\tmp\o", "service", "start"])
			.expect("should parse");

		assert_eq!(cli.elevated_output.expect("set").to_string_lossy(), r"C:\tmp\o");
		assert!(needs_elevation(&cli.command), "still the command it wraps");
	}

	#[test]
	fn a_bad_expiry_is_rejected_before_the_daemon_is_ever_contacted() {
		// `check_args` runs ahead of both the elevation check and any IPC, so a
		// command that cannot work is refused without a prompt, a connection, or
		// a confusing error from the far end.
		let cli = Cli::try_parse_from(["quix", "invite", "--expires", "5", "fortnights"])
			.expect("clap accepts the shape; the unit is ours to judge");

		assert!(check_args(&cli.command).is_err());
	}

	#[test]
	fn a_good_expiry_passes_the_check() {
		for form in [
			vec!["quix", "invite"],
			vec!["quix", "invite", "--expires", "30", "min"],
			vec!["quix", "invite", "--expires", "2", "days"],
		] {
			let cli = Cli::try_parse_from(&form).expect("should parse");
			assert!(check_args(&cli.command).is_ok(), "{form:?}");
		}
	}

	#[test]
	fn expires_needs_both_an_amount_and_a_unit() {
		assert!(Cli::try_parse_from(["quix", "invite", "--expires", "30"]).is_err());
	}

	#[test]
	fn the_output_flag_is_hidden_from_help() {
		// It is plumbing between two copies of this program, not something to type.
		let help = Cli::command().render_long_help().to_string();
		assert!(!help.contains("elevated-output"), "{help}");
	}
}