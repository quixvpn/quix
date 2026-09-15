//! What `quixd` was asked to do.
//!
//! The daemon used to read its command line not at all: every argument fell
//! through to starting up, so `quixd version` booted a second daemon instead of
//! printing one — a fresh identity, its own key under the invoking user's home,
//! contending with the service for the interface. An argument that means nothing
//! to us is now refused rather than treated as no argument at all.
//!
//! There is deliberately nothing here that changes how the daemon runs. Paths
//! and addresses come from the environment (`QUIX_KEY_PATH`, `QUIX_DNS_ADDR`,
//! and so on) so that a service manager holds them, and everything a person
//! would want to do lives in `quix`.

use anyhow::{bail, Result};

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
	/// Run the daemon. What a bare `quixd` means, and the only thing that
	/// starts anything.
	Run,
	/// Print the version and exit.
	Version,
	/// Print [`USAGE`] and exit.
	Help,
}

/// Printed for `--help`, and again with any refusal: someone who guessed wrong
/// wants to see what would have been right.
pub const USAGE: &str = "\
quixd — the quix mesh daemon

usage:
  quixd              run the daemon in the foreground
  quixd --version    print the version and exit
  quixd --help       print this and exit

The daemon is normally started by a service manager rather than by hand, and is
configured through the environment, not through options. Everything else is a
`quix` subcommand: `quix status`, `quix create`, `quix service restart`.";

/// Reads the command line, with the program name already dropped.
pub fn parse<I, S>(args: I) -> Result<Invocation>
where
	I: IntoIterator<Item = S>,
	S: AsRef<str>,
{
	let args: Vec<S> = args.into_iter().collect();

	match args.as_slice() {
		[] => Ok(Invocation::Run),
		// Both spellings of each, because the bug that started this was someone
		// reasonably typing the bare word.
		[one] => match one.as_ref() {
			"--version" | "-V" | "version" => Ok(Invocation::Version),
			"--help" | "-h" | "help" => Ok(Invocation::Help),
			other => bail!("unrecognized argument `{other}`\n\n{USAGE}"),
		},
		// Naming every extra one says no more than naming the count: nothing
		// here takes a value, so a second argument is always a misunderstanding
		// rather than a typo.
		extra => bail!(
			"expected at most one argument, got {}\n\n{USAGE}",
			extra.len()
		),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The service manager starts us with nothing, and so does a person who
	/// wants a daemon.
	#[test]
	fn no_arguments_runs_the_daemon() {
		assert_eq!(parse::<_, &str>([]).unwrap(), Invocation::Run);
	}

	#[test]
	fn the_bare_word_version_prints_a_version_rather_than_starting_a_daemon() {
		// The actual bug: `quixd version` came back with a startup banner, a new
		// identity and a key file in ~/.config, because the argument was ignored
		// and the daemon started as if none had been given.
		assert_eq!(parse(["version"]).unwrap(), Invocation::Version);
		assert_eq!(parse(["--version"]).unwrap(), Invocation::Version);
		assert_eq!(parse(["-V"]).unwrap(), Invocation::Version);
	}

	#[test]
	fn help_is_accepted_in_the_same_three_spellings() {
		assert_eq!(parse(["help"]).unwrap(), Invocation::Help);
		assert_eq!(parse(["--help"]).unwrap(), Invocation::Help);
		assert_eq!(parse(["-h"]).unwrap(), Invocation::Help);
	}

	#[test]
	fn an_unrecognized_argument_is_refused_instead_of_ignored() {
		// Refusing matters more than the message: ignoring it is what started a
		// whole second daemon.
		let error = parse(["--daemonize"]).expect_err("must not be taken for a bare invocation");

		let message = error.to_string();
		assert!(message.contains("--daemonize"), "names what it refused: {message}");
		assert!(message.contains("usage:"), "shows what would have worked: {message}");
	}

	#[test]
	fn a_misspelled_version_is_refused_rather_than_starting_a_daemon() {
		assert!(parse(["--verison"]).is_err());
		assert!(parse(["-v"]).is_err(), "lowercase -v is not the version flag");
	}

	#[test]
	fn more_than_one_argument_is_refused() {
		let error = parse(["--version", "--help"]).expect_err("nothing here takes a value");
		assert!(error.to_string().contains("at most one"), "{error}");
	}
}
