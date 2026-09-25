//! Whether a name a peer sent us is safe to create a file under.
//!
//! The name in a file offer comes from another machine, so it is judged, never
//! repaired: a legitimate sender only ever sends the last component of a real
//! path, and anything that fails here is either a bug or an attempt to write
//! somewhere other than where the user asked. Silently rewriting it would hide
//! which of those it was.
//!
//! One function for both sides. The receiving daemon runs it when an offer
//! arrives, so the sender hears why at once; the receiving CLI runs it again
//! before creating anything, because the CLI is what actually writes.

/// The rules differ by filesystem, so the platform is a parameter rather than a
/// `cfg`: the Windows rules then get exercised by tests on every host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
	Unix,
	Windows,
}

impl Platform {
	/// The platform this binary is running on.
	pub fn current() -> Self {
		if cfg!(windows) {
			Platform::Windows
		} else {
			Platform::Unix
		}
	}
}

/// The longest name accepted, in bytes. The common limit for a single path
/// component on Linux filesystems and NTFS alike.
pub const MAX_LEN: usize = 255;

/// Device names Windows resolves anywhere, whatever the directory and whatever
/// the extension: `CON.txt` opens the console, not a file.
const WINDOWS_RESERVED: &[&str] = &[
	"CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM1", "COM2", "COM3", "COM4", "COM5",
	"COM6", "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5",
	"LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
];

/// Characters NTFS refuses in a name. `:` is the one that matters most: it
/// names an alternate data stream rather than failing.
const WINDOWS_INVALID: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

/// Returns why `name` cannot be used as a file name here, or `Ok` if it can.
pub fn check(name: &str, platform: Platform) -> Result<(), String> {
	if name.is_empty() {
		return Err("the file name is empty".to_string());
	}
	if name == "." || name == ".." {
		return Err(format!("{name:?} is not a file name"));
	}
	// Both separators on every platform: a sender only ever sends the last
	// component of a path, so a separator of either kind means someone is
	// trying to steer where the file lands.
	if name.contains('/') || name.contains('\\') {
		return Err(format!("{name:?} contains a path separator"));
	}
	if name.len() > MAX_LEN {
		return Err(format!(
			"the file name is {} bytes; the limit is {MAX_LEN}",
			name.len()
		));
	}
	if let Some(bad) = name.chars().find(|c| is_control(*c)) {
		return Err(format!(
			"the file name contains the control character U+{:04X}",
			bad as u32
		));
	}

	if platform == Platform::Windows {
		if let Some(bad) = name.chars().find(|c| WINDOWS_INVALID.contains(c)) {
			return Err(format!(
				"the file name contains {bad:?}, which Windows does not allow"
			));
		}
		if name.ends_with('.') || name.ends_with(' ') {
			// Windows strips these when creating the file, so the name written
			// would not be the name checked.
			return Err("the file name ends in a dot or a space, which Windows drops".to_string());
		}
		// The device is matched on what comes before the first dot, with the
		// trailing spaces Windows ignores removed: `con .txt` is still CON.
		let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
		if WINDOWS_RESERVED
			.iter()
			.any(|reserved| reserved.eq_ignore_ascii_case(stem))
		{
			return Err(format!("{name:?} is a reserved device name on Windows"));
		}
	}

	Ok(())
}

/// Control characters, plus the invisible bidirectional overrides that make
/// `invoice<U+202E>fdp.exe` display as `invoiceexe.pdf`. Those are formatting
/// characters to Unicode rather than controls, but they control how the name
/// reads, and a name that reads as something else is the problem.
fn is_control(c: char) -> bool {
	c.is_control()
		|| matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// The `n`th alternative to `name` when it is taken: `photo.jpg` becomes
/// `photo (1).jpg`, the form every file manager already uses.
///
/// The number goes before the extension so the file still opens with the same
/// program. A leading dot is part of the name, not an extension, so `.bashrc`
/// becomes `.bashrc (1)`. The result is kept within [`MAX_LEN`] by shortening
/// the stem, never the extension or the number.
pub fn numbered(name: &str, n: u32) -> String {
	let (stem, ext) = match name.rfind('.') {
		Some(dot) if dot > 0 => name.split_at(dot),
		_ => (name, ""),
	};
	let tag = format!(" ({n})");

	let room = MAX_LEN.saturating_sub(tag.len() + ext.len());
	let mut cut = stem.len().min(room);
	while !stem.is_char_boundary(cut) {
		cut -= 1;
	}

	format!("{}{tag}{ext}", &stem[..cut])
}

#[cfg(test)]
mod tests {
	use super::*;

	fn both(name: &str) -> (Result<(), String>, Result<(), String>) {
		(check(name, Platform::Unix), check(name, Platform::Windows))
	}

	#[test]
	fn ordinary_names_pass_everywhere() {
		for name in [
			"photo.jpg",
			"report final.pdf",
			".bashrc",
			"archive.tar.gz",
			"ação.txt",
			"日本.txt",
			"a",
		] {
			let (unix, windows) = both(name);
			assert_eq!(unix, Ok(()), "{name:?} on unix");
			assert_eq!(windows, Ok(()), "{name:?} on windows");
		}
	}

	#[test]
	fn empty_and_dot_names_are_refused() {
		for name in ["", ".", ".."] {
			let (unix, windows) = both(name);
			assert!(unix.is_err() && windows.is_err(), "{name:?}");
		}
	}

	#[test]
	fn any_separator_is_refused_rather_than_stripped() {
		// A legitimate sender strips directories before sending, so a name that
		// still has one is refused outright — including a lone backslash on
		// Unix, where it would be a legal but deliberately confusing name.
		for name in [
			"../../etc/passwd",
			"..",
			"/etc/passwd",
			"dir/file.txt",
			r"..\..\Windows\System32\evil.dll",
			r"C:\Users\x\evil.exe",
			r"\\server\share\f",
			"a\\b",
		] {
			let (unix, windows) = both(name);
			assert!(unix.is_err(), "{name:?} on unix");
			assert!(windows.is_err(), "{name:?} on windows");
		}
	}

	#[test]
	fn a_drive_letter_is_refused_on_windows() {
		// `C:evil.txt` is drive-relative on Windows, not a file name.
		assert!(check("C:evil.txt", Platform::Windows).is_err());
	}

	#[test]
	fn control_characters_are_refused() {
		for name in [
			"a\0b",
			"line\nbreak",
			"tab\there",
			"bell\u{7}",
			"del\u{7f}",
			"c1\u{85}",
		] {
			let (unix, windows) = both(name);
			assert!(unix.is_err() && windows.is_err(), "{name:?}");
		}
	}

	#[test]
	fn bidirectional_overrides_are_refused() {
		// Displays as "invoiceexe.pdf".
		let spoof = "invoice\u{202E}fdp.exe";
		assert!(check(spoof, Platform::Unix).is_err());
		assert!(check(spoof, Platform::Windows).is_err());
	}

	#[test]
	fn overlong_names_are_refused() {
		assert!(
			check(&"a".repeat(MAX_LEN), Platform::Unix).is_ok(),
			"exactly at the limit"
		);
		assert!(check(&"a".repeat(MAX_LEN + 1), Platform::Unix).is_err());
		// Measured in bytes: 128 two-byte characters are 256 bytes.
		assert!(check(&"é".repeat(128), Platform::Unix).is_err());
	}

	#[test]
	fn windows_device_names_are_refused_with_any_case_or_extension() {
		for name in [
			"CON",
			"con",
			"Con.txt",
			"NUL",
			"nul.tar.gz",
			"PRN",
			"AUX",
			"COM1",
			"com9.log",
			"LPT1",
			"lpt9",
			"CONIN$",
			"conout$",
			"COM¹",
			"LPT³.txt",
			"con .txt",
		] {
			assert!(check(name, Platform::Windows).is_err(), "{name:?}");
		}
		// Harmless on Unix, where they are ordinary names.
		assert!(check("CON", Platform::Unix).is_ok());
		assert!(check("nul.txt", Platform::Unix).is_ok());
	}

	#[test]
	fn names_that_merely_start_like_a_device_are_fine() {
		for name in [
			"CONSOLE.txt",
			"COM10",
			"LPT0",
			"auxiliary",
			"nullable.rs",
			"COM",
		] {
			assert_eq!(check(name, Platform::Windows), Ok(()), "{name:?}");
		}
	}

	#[test]
	fn characters_ntfs_refuses_are_refused_on_windows_only() {
		for name in ["a<b", "a>b", "a:b", "a\"b", "a|b", "what?", "star*"] {
			assert!(
				check(name, Platform::Windows).is_err(),
				"{name:?} on windows"
			);
			assert_eq!(check(name, Platform::Unix), Ok(()), "{name:?} on unix");
		}
	}

	#[test]
	fn trailing_dots_and_spaces_are_refused_on_windows() {
		for name in ["file.", "file ", "file. ."] {
			assert!(check(name, Platform::Windows).is_err(), "{name:?}");
			assert_eq!(check(name, Platform::Unix), Ok(()), "{name:?}");
		}
	}

	#[test]
	fn numbering_goes_before_the_extension() {
		assert_eq!(numbered("photo.jpg", 1), "photo (1).jpg");
		assert_eq!(numbered("photo.jpg", 12), "photo (12).jpg");
		assert_eq!(numbered("archive.tar.gz", 2), "archive.tar (2).gz");
		assert_eq!(numbered("README", 1), "README (1)");
	}

	#[test]
	fn a_leading_dot_is_not_an_extension() {
		assert_eq!(numbered(".bashrc", 1), ".bashrc (1)");
		assert_eq!(numbered(".config.json", 1), ".config (1).json");
	}

	#[test]
	fn a_numbered_name_is_still_a_valid_name() {
		let long = format!("{}.txt", "é".repeat(125));
		assert!(
			check(&long, Platform::Unix).is_ok(),
			"the input itself is valid"
		);

		let renamed = numbered(&long, 9999);
		assert!(renamed.len() <= MAX_LEN, "{} bytes", renamed.len());
		assert!(renamed.ends_with(" (9999).txt"), "{renamed}");
		assert_eq!(check(&renamed, Platform::Unix), Ok(()));
	}
}
