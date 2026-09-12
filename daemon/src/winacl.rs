//! Locking a file down so only this machine's administrators can read it.
//!
//! Windows has no `chmod`. A file created under `C:\ProgramData` inherits that
//! directory's permissions, which grant `BUILTIN\Users` read — fine for the
//! roster, fatal for the secret key, which is the node's entire identity on the
//! mesh. Replacing the inherited list with an explicit one is the equivalent of
//! the `0o600` the Unix side already applies.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use anyhow::{Context, Result};
use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
use windows_sys::Win32::Security::Authorization::{
	ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SDDL_REVISION_1,
	SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
	GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
	PSECURITY_DESCRIPTOR,
};

/// The SID of `NT AUTHORITY\SYSTEM`, which the `SY` shorthand already covers.
const LOCAL_SYSTEM: &str = "S-1-5-18";

/// Restricts `path` to SYSTEM, the Administrators group, and whoever we are.
///
/// `D:P` is the part that matters: `P` protects the list, so the permissive
/// entries inherited from `ProgramData` are dropped rather than merged with.
pub fn restrict_to_administrators(path: &Path) -> Result<()> {
	let mut sddl = String::from("D:P(A;;FA;;;SY)(A;;FA;;;BA)");

	// A daemon run straight from a build tree is neither SYSTEM nor an
	// administrator, and locking it out of the key it just wrote helps nobody.
	if let Some(sid) = crate::winauth::current_process().map(|me| me.sid) {
		// The SID comes from the OS, but it is being pasted into a language
		// with its own syntax, so it is checked rather than trusted.
		if sid != LOCAL_SYSTEM && is_sid(&sid) {
			sddl.push_str(&format!("(A;;FA;;;{sid})"));
		}
	}

	apply(path, &sddl).with_context(|| format!("restricting access to {}", path.display()))
}

/// Whether a string is shaped like a SID and nothing else.
fn is_sid(text: &str) -> bool {
	text.starts_with("S-1-")
		&& text.len() <= 184
		&& text.chars().all(|c| c.is_ascii_digit() || c == 'S' || c == '-')
}

fn apply(path: &Path, sddl: &str) -> Result<()> {
	let wide_sddl = wide(sddl.as_ref());
	let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();

	// SAFETY: `descriptor` receives a heap-allocated security descriptor only
	// when this reports success; it is freed below on every path.
	if unsafe {
		ConvertStringSecurityDescriptorToSecurityDescriptorW(
			wide_sddl.as_ptr(),
			SDDL_REVISION_1,
			&mut descriptor,
			ptr::null_mut(),
		)
	} == 0
	{
		return Err(std::io::Error::last_os_error()).context("building the permission list");
	}

	let result = set_dacl(path, descriptor);

	// SAFETY: allocated by the call above, freed exactly once, not used after.
	unsafe { LocalFree(descriptor.cast()) };
	result
}

fn set_dacl(path: &Path, descriptor: PSECURITY_DESCRIPTOR) -> Result<()> {
	let mut dacl: *mut ACL = ptr::null_mut();
	let (mut present, mut defaulted) = (0, 0);

	// SAFETY: `descriptor` is a live descriptor; `dacl` borrows from it and is
	// used only while it is alive.
	if unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) } == 0
	{
		return Err(std::io::Error::last_os_error()).context("reading the permission list back");
	}
	if present == 0 {
		anyhow::bail!("the permission list came back empty");
	}

	let mut wide_path = wide(path.as_os_str());

	// SAFETY: a nul-terminated path and a DACL borrowed from a live descriptor.
	let status = unsafe {
		SetNamedSecurityInfoW(
			wide_path.as_mut_ptr(),
			SE_FILE_OBJECT,
			// PROTECTED is what actually severs inheritance; without it the
			// permissive entries from ProgramData would be merged back in.
			DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
			ptr::null_mut(),
			ptr::null_mut(),
			dacl,
			ptr::null_mut(),
		)
	};

	match status == ERROR_SUCCESS {
		true => Ok(()),
		false => Err(std::io::Error::from_raw_os_error(status as i32))
			.context("applying the permission list"),
	}
}

fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
	text.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_real_sid_is_accepted_and_anything_else_is_not() {
		assert!(is_sid("S-1-5-21-1-2-3-1001"));
		assert!(is_sid(LOCAL_SYSTEM));
		// SDDL is a language; a SID is the only thing allowed to reach it.
		assert!(!is_sid("S-1-5-21-1)(A;;FA;;;WD"), "would widen the list");
		assert!(!is_sid("BA"));
		assert!(!is_sid(""));
		assert!(!is_sid(&format!("S-1-{}", "5-".repeat(200))), "absurdly long");
	}

	/// The claim is that a file ends up unreadable by ordinary users, so the
	/// test applies it to a real file and reads the permissions back.
	#[test]
	fn a_restricted_file_lists_nobody_but_administrators_and_us() {
		let path = std::env::temp_dir().join(format!("quix-acl-{}", std::process::id()));
		std::fs::write(&path, b"secret").unwrap();

		restrict_to_administrators(&path).expect("should restrict");

		let shown = std::process::Command::new("icacls")
			.arg(&path)
			.output()
			.expect("icacls");
		let text = String::from_utf8_lossy(&shown.stdout);
		let _ = std::fs::remove_file(&path);

		// `Users` is exactly the entry ProgramData hands out and the one the
		// key must never carry.
		assert!(
			!text.contains("BUILTIN\\Users") && !text.contains("\\Everyone"),
			"still readable by ordinary users:\n{text}"
		);
		assert!(text.contains("BUILTIN\\Administrators"), "{text}");
	}
}
