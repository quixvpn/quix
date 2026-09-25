//! Where a received file goes. Decided here, in the CLI, for the user running
//! it: the daemon's idea of a home or working directory is root's or SYSTEM's,
//! which is meaningless to whoever typed the command.

use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Debug, PartialEq, Eq)]
pub struct Destination {
	pub dir: PathBuf,
	/// Something the user should be told about the choice, when it was not the
	/// one they would expect.
	pub note: Option<String>,
}

/// The directory to save into, for the user running this process.
pub fn for_caller(here: bool) -> Result<Destination> {
	resolve(
		here,
		std::env::current_dir,
		dirs::download_dir(),
		dirs::home_dir(),
	)
}

/// The rule itself, with every input passed in so it can be tested without
/// depending on who runs the tests or where.
///
/// `--here` is the directory the command was run from. Otherwise it is the
/// user's Downloads folder, on every platform; if the platform names none, the
/// home directory, and the user is told.
pub fn resolve(
	here: bool,
	cwd: impl FnOnce() -> std::io::Result<PathBuf>,
	downloads: Option<PathBuf>,
	home: Option<PathBuf>,
) -> Result<Destination> {
	if here {
		let dir = cwd().context("could not tell which directory this is")?;
		return Ok(Destination { dir, note: None });
	}

	match (downloads, home) {
		(Some(dir), _) => Ok(Destination { dir, note: None }),
		(None, Some(home)) => Ok(Destination {
			note: Some(format!(
				"no Downloads folder is configured, so saving to {}",
				home.display()
			)),
			dir: home,
		}),
		(None, None) => anyhow::bail!(
			"there is no Downloads folder or home directory to save into — use --here to save \
			 into the current directory"
		),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn cwd() -> std::io::Result<PathBuf> {
		Ok(PathBuf::from("/work/project"))
	}

	fn no_cwd() -> std::io::Result<PathBuf> {
		Err(std::io::ErrorKind::NotFound.into())
	}

	fn downloads() -> Option<PathBuf> {
		Some(PathBuf::from("/home/someone/Downloads"))
	}

	fn home() -> Option<PathBuf> {
		Some(PathBuf::from("/home/someone"))
	}

	#[test]
	fn by_default_files_go_to_downloads() {
		let dest = resolve(false, cwd, downloads(), home()).unwrap();
		assert_eq!(dest.dir, PathBuf::from("/home/someone/Downloads"));
		assert_eq!(dest.note, None);
	}

	#[test]
	fn here_means_the_directory_the_command_was_run_from() {
		let dest = resolve(true, cwd, downloads(), home()).unwrap();
		assert_eq!(dest.dir, PathBuf::from("/work/project"));
		assert_eq!(
			dest.note, None,
			"asked for explicitly, so nothing to explain"
		);
	}

	#[test]
	fn here_does_not_fall_back_to_anywhere_else() {
		// Saving somewhere other than where the user said would be worse than
		// failing.
		assert!(resolve(true, no_cwd, downloads(), home()).is_err());
	}

	#[test]
	fn with_no_downloads_folder_the_home_directory_is_used_and_the_user_told() {
		let dest = resolve(false, cwd, None, home()).unwrap();
		assert_eq!(dest.dir, PathBuf::from("/home/someone"));
		let note = dest.note.expect("the fallback is announced");
		assert!(note.contains("/home/someone"), "{note}");
	}

	#[test]
	fn with_neither_the_user_is_pointed_at_here() {
		let refused = resolve(false, cwd, None, None).unwrap_err().to_string();
		assert!(refused.contains("--here"), "{refused}");
	}

	#[test]
	fn the_real_caller_resolves_here_to_the_process_working_directory() {
		let dest = for_caller(true).unwrap();
		assert_eq!(dest.dir, std::env::current_dir().unwrap());
	}
}
