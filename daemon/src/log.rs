//! Logging that survives running as a service.
//!
//! A Windows service has no console, so everything written to stdout is
//! discarded — and unlike Linux, where journald captures it, there is nowhere
//! else for it to go. Running the binary by hand is not a substitute either:
//! the service's state paths come from its registry environment, so a manual
//! run loads a different identity entirely and is a different node.
//!
//! So every message goes to the console *and*, when `QUIX_LOG_PATH` is set, to
//! a file. The installer points the Windows service at one.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Opened once on first use. `None` means no file was configured, or opening it
/// failed — in which case console output carries on alone, because losing logs
/// is not a reason to stop carrying traffic.
static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

pub fn path() -> Option<PathBuf> {
	std::env::var("QUIX_LOG_PATH").ok().map(PathBuf::from)
}

fn file() -> Option<&'static Mutex<std::fs::File>> {
	FILE.get_or_init(|| {
		let path = path()?;
		if let Some(parent) = path.parent() {
			let _ = std::fs::create_dir_all(parent);
		}
		match OpenOptions::new().create(true).append(true).open(&path) {
			Ok(file) => Some(Mutex::new(file)),
			Err(e) => {
				// Straight to stderr: the logger cannot report its own failure.
				eprintln!("could not open {}: {e}", path.display());
				None
			}
		}
	})
	.as_ref()
}

/// Writes one line to the console, and to the log file when there is one.
///
/// Called by the `info!` and `warn!` macros; not meant to be used directly.
pub fn write(level: &str, message: &str) {
	match level {
		"WARN" => eprintln!("{message}"),
		_ => println!("{message}"),
	}

	let Some(file) = file() else { return };
	let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");

	if let Ok(mut file) = file.lock() {
		let _ = writeln!(file, "{stamp} {level:<5} {message}");
		let _ = file.flush();
	}
}

/// Ordinary progress, to stdout.
#[macro_export]
macro_rules! info {
	($($arg:tt)*) => { $crate::log::write("INFO", &format!($($arg)*)) };
}

/// Something the operator should see, to stderr.
#[macro_export]
macro_rules! warn {
	($($arg:tt)*) => { $crate::log::write("WARN", &format!($($arg)*)) };
}
