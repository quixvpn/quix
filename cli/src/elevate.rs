//! Asking Windows for Administrator when a command cannot work without it.
//!
//! There is no `sudo` here: a running process cannot gain privileges, it can
//! only start a new one that has them. `ShellExecuteExW` with the `runas` verb
//! is the same mechanism behind the shell's own "Run as administrator", and it
//! is what raises the UAC prompt.
//!
//! The elevated child cannot share this console — Windows does not let a
//! higher-integrity process attach to a lower-integrity one — so it is started
//! hidden with its output pointed at a file, which we print here once it exits.
//! From the terminal it looks like the command simply ran, after a prompt.

use std::fs::OpenOptions;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{Context, Result};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE};
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::Threading::{
	GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::UI::Shell::{
	ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

/// The flag the parent passes to the elevated child. Hidden from `--help`: it
/// is plumbing between two copies of this program, not something to type.
pub const OUTPUT_FLAG: &str = "--elevated-output";

/// Whether this process already holds Administrator rights.
pub fn is_elevated() -> bool {
	let mut token: HANDLE = ptr::null_mut();
	// SAFETY: `token` is written only when this reports success.
	if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
		return false;
	}

	let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
	let mut size = 0u32;
	// SAFETY: the buffer is exactly one TOKEN_ELEVATION, and its size says so.
	let ok = unsafe {
		GetTokenInformation(
			token,
			TokenElevation,
			ptr::addr_of_mut!(elevation).cast(),
			size_of::<TOKEN_ELEVATION>() as u32,
			&mut size,
		)
	};
	// SAFETY: opened above, not used again.
	unsafe { CloseHandle(token) };

	ok != 0 && elevation.TokenIsElevated != 0
}

/// Re-runs this exact command as Administrator and reproduces its result here.
///
/// Never returns: the child's output becomes ours, and so does its exit code.
pub fn relaunch() -> ! {
	match run_elevated() {
		Ok(code) => std::process::exit(code),
		Err(e) => {
			eprintln!("{e:#}");
			std::process::exit(1);
		}
	}
}

fn run_elevated() -> Result<i32> {
	let exe = std::env::current_exe().context("locating the running executable")?;
	let output = output_path();

	// The output flag goes first so it is parsed as a top-level option, ahead of
	// the subcommand. Everything else is passed through untouched.
	let mut args = vec![OUTPUT_FLAG.to_string(), output.display().to_string()];
	args.extend(std::env::args().skip(1));

	let code = shell_execute_runas(&exe, &join_args(&args));
	// Print whatever the child managed to write, even if it then failed: its
	// own error message is the useful part.
	show(&output);
	let _ = std::fs::remove_file(&output);

	code
}

/// Sends this process's output to `path`, for the parent that is waiting on it.
///
/// Must run before anything is written. Both streams go to the same file so
/// that ordering between them survives the round trip.
pub fn redirect_output(path: &Path) -> Result<()> {
	let file = OpenOptions::new()
		.create(true)
		.append(true)
		.open(path)
		.with_context(|| format!("opening {}", path.display()))?;
	let handle = file.as_raw_handle() as HANDLE;

	for stream in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
		// SAFETY: a live file handle, kept alive by the leak below.
		if unsafe { SetStdHandle(stream, handle) } == 0 {
			return Err(std::io::Error::last_os_error()).context("redirecting output");
		}
	}

	// Deliberately leaked: the handle has to outlive every later write, and this
	// process does all its work and exits without another chance to close it.
	std::mem::forget(file);
	Ok(())
}

fn show(path: &Path) {
	let Ok(text) = std::fs::read_to_string(path) else {
		return;
	};
	print!("{text}");
}

/// Somewhere only this run writes to. The pid keeps two concurrent elevations
/// from reading each other's output.
fn output_path() -> PathBuf {
	std::env::temp_dir().join(format!("quix-elevated-{}.out", std::process::id()))
}

fn shell_execute_runas(exe: &Path, params: &str) -> Result<i32> {
	let verb = wide("runas");
	let file = wide(&exe.to_string_lossy());
	let params = wide(params);
	let dir = std::env::current_dir()
		.map(|d| wide(&d.to_string_lossy()))
		.unwrap_or_else(|_| wide(""));

	// SAFETY: zeroed is the documented starting state; every field used below is
	// set before the call.
	let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
	info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
	// NOCLOSEPROCESS hands back a handle to wait on — without it there is no way
	// to know whether the command worked. NO_UI stops Windows putting its own
	// error box on top of the message we are about to print.
	info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI;
	info.lpVerb = verb.as_ptr();
	info.lpFile = file.as_ptr();
	info.lpParameters = params.as_ptr();
	info.lpDirectory = dir.as_ptr();
	// Hidden because the child writes to a file rather than to a console; a
	// window here would only flash and vanish.
	info.nShow = SW_HIDE;

	// SAFETY: `info` is fully initialised and the wide strings outlive the call.
	if unsafe { ShellExecuteExW(&mut info) } == 0 {
		let err = std::io::Error::last_os_error();
		// Declining the prompt is a decision, not a crash.
		if err.raw_os_error() == Some(ERROR_CANCELLED as i32) {
			anyhow::bail!("elevation was declined — this command needs Administrator");
		}
		return Err(err).context("could not ask for Administrator");
	}

	wait_for(info.hProcess)
}

fn wait_for(process: HANDLE) -> Result<i32> {
	// SAFETY: a live process handle, owned by us because of NOCLOSEPROCESS.
	unsafe { WaitForSingleObject(process, INFINITE) };

	let mut code = 0u32;
	// SAFETY: same handle, still open.
	let ok = unsafe { GetExitCodeProcess(process, &mut code) };
	// SAFETY: ours to close, and not used again.
	unsafe { CloseHandle(process) };

	match ok {
		0 => anyhow::bail!("the elevated command ran but its result could not be read"),
		_ => Ok(code as i32),
	}
}

fn wide(s: &str) -> Vec<u16> {
	s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Joins arguments the way `CommandLineToArgvW` will split them apart again.
///
/// `ShellExecuteExW` takes one string rather than a list, so this has to be
/// exact: a network name containing a space would otherwise arrive as two
/// arguments, and one containing a quote could inject a third.
fn join_args(args: &[String]) -> String {
	args.iter()
		.map(|arg| quote(arg))
		.collect::<Vec<_>>()
		.join(" ")
}

fn quote(arg: &str) -> String {
	if !arg.is_empty() && !arg.contains([' ', '\t', '"', '\\']) {
		return arg.to_string();
	}

	let mut out = String::from("\"");
	let mut backslashes = 0;

	for c in arg.chars() {
		match c {
			'\\' => {
				backslashes += 1;
				out.push(c);
			}
			'"' => {
				// Backslashes immediately before a quote are doubled, and the
				// quote itself escaped, or the parser would end the argument.
				out.push_str(&"\\".repeat(backslashes + 1));
				out.push('"');
				backslashes = 0;
			}
			_ => {
				backslashes = 0;
				out.push(c);
			}
		}
	}

	// A trailing backslash would otherwise escape the closing quote.
	out.push_str(&"\\".repeat(backslashes));
	out.push('"');
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Splits a command line the way Windows does, so quoting can be checked
	/// against the thing that will actually parse it.
	fn split(line: &str) -> Vec<String> {
		let mut args = Vec::new();
		let mut current = String::new();
		let mut chars = line.chars().peekable();
		let (mut in_quotes, mut started) = (false, false);

		while let Some(c) = chars.next() {
			match c {
				'\\' => {
					let mut slashes = 1;
					while chars.peek() == Some(&'\\') {
						chars.next();
						slashes += 1;
					}
					if chars.peek() == Some(&'"') {
						current.push_str(&"\\".repeat(slashes / 2));
						if slashes % 2 == 1 {
							chars.next();
							current.push('"');
						}
					} else {
						current.push_str(&"\\".repeat(slashes));
					}
				}
				'"' => {
					in_quotes = !in_quotes;
					started = true;
				}
				' ' | '\t' if !in_quotes => {
					if started || !current.is_empty() {
						args.push(std::mem::take(&mut current));
						started = false;
					}
				}
				_ => current.push(c),
			}
		}
		if started || !current.is_empty() {
			args.push(current);
		}
		args
	}

	fn round_trip(args: &[&str]) {
		let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
		assert_eq!(split(&join_args(&owned)), owned, "line: {}", join_args(&owned));
	}

	#[test]
	fn plain_arguments_survive_unchanged() {
		round_trip(&["service", "start"]);
		assert_eq!(join_args(&["service".into(), "start".into()]), "service start");
	}

	#[test]
	fn an_argument_with_spaces_stays_one_argument() {
		// A network or hostname is user input and can contain anything.
		round_trip(&["create", "my network"]);
	}

	#[test]
	fn quotes_and_backslashes_survive() {
		round_trip(&["hostname", r#"od"d"#]);
		round_trip(&["--elevated-output", r"C:\Users\a b\quix.out"]);
		round_trip(&[r"trailing\\"]);
		round_trip(&[r#"a\"b"#]);
	}

	#[test]
	fn an_empty_argument_is_preserved() {
		// Dropping it would shift every later argument by one.
		round_trip(&["hostname", ""]);
	}

	#[test]
	fn the_output_flag_leads_so_clap_reads_it_before_the_subcommand() {
		let args = [OUTPUT_FLAG.to_string(), "C:\\tmp\\o".to_string(), "service".to_string()];
		assert!(join_args(&args).starts_with(OUTPUT_FLAG));
	}
}
