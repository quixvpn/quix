//! Free space where a file is about to land, so a transfer that cannot fit is
//! refused before it starts rather than after gigabytes have crossed the
//! network. `None` when the platform cannot say, in which case the write itself
//! fails if the space runs out.

use std::path::Path;

#[cfg(unix)]
pub fn available(dir: &Path) -> Option<u64> {
	use std::ffi::CString;
	use std::os::unix::ffi::OsStrExt;

	let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
	let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
	// SAFETY: a nul-terminated path and a struct of the type statvfs fills.
	if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
		return None;
	}
	// Blocks available to an unprivileged user, not the root reserve.
	(stats.f_bavail as u64).checked_mul(stats.f_frsize as u64)
}

#[cfg(windows)]
pub fn available(dir: &Path) -> Option<u64> {
	use std::os::windows::ffi::OsStrExt;
	use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

	let wide: Vec<u16> = dir
		.as_os_str()
		.encode_wide()
		.chain(std::iter::once(0))
		.collect();
	let mut available: u64 = 0;
	// SAFETY: a nul-terminated path; the optional totals are not asked for.
	// "Available to the caller" honours disk quotas, which the total would not.
	let ok = unsafe {
		GetDiskFreeSpaceExW(
			wide.as_ptr(),
			&mut available,
			std::ptr::null_mut(),
			std::ptr::null_mut(),
		)
	};
	(ok != 0).then_some(available)
}

#[cfg(not(any(unix, windows)))]
pub fn available(_dir: &Path) -> Option<u64> {
	None
}

/// Refuses early when `size` more bytes will not fit in `dir`.
pub fn check(dir: &Path, size: u64) -> anyhow::Result<()> {
	match available(dir) {
		Some(free) if free < size => anyhow::bail!(
			"not enough space in {}: the file is {}, and {} is free",
			dir.display(),
			super::format::size(size),
			super::format::size(free)
		),
		_ => Ok(()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_temp_directory_has_some_space_and_not_all_of_it() {
		let free = available(&std::env::temp_dir());
		if cfg!(any(unix, windows)) {
			let free = free.expect("the platform can say");
			assert!(free > 0 && free < u64::MAX);
		}
	}

	#[test]
	fn a_file_larger_than_the_disk_is_refused_before_anything_moves() {
		let refused = check(&std::env::temp_dir(), u64::MAX)
			.unwrap_err()
			.to_string();
		assert!(refused.contains("not enough space"), "{refused}");
		assert!(check(&std::env::temp_dir(), 1).is_ok());
	}
}
