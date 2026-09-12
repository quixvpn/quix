//! Reading the identity of the process on the other end of a named pipe.
//!
//! Windows has no `SO_PEERCRED`. The pipe can name the client's PID, but
//! turning a PID into a token is racy — the process can exit and its PID be
//! reused between the lookup and the check, which interprocess's own
//! documentation warns against relying on. Impersonating the client instead
//! asks the kernel about *this connection*, so there is nothing to race and
//! nothing the client can claim for itself.

use std::os::windows::io::{AsHandle, AsRawHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
	GetTokenInformation, LookupAccountSidW, RevertToSelf, TokenElevation, TokenUser,
	TOKEN_ELEVATION, TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

/// What the kernel says about the client on the other end of a connection.
pub struct PipeClient {
	/// The client's user SID, e.g. `S-1-5-21-…-1001`. Survives renames, which
	/// is why it and not the account name is the identity we keep.
	pub sid: String,
	/// `DOMAIN\User`, when it resolves. For messages only.
	pub account: Option<String>,
	/// Whether the client's token is elevated — the Windows stand-in for root.
	pub elevated: bool,
}

/// Reads the client's identity off a live connection.
///
/// `None` means there is no identity to be had, which callers must treat as
/// unprivileged rather than as a reason to skip the check: a client is free to
/// connect at the anonymous impersonation level, and then the token cannot be
/// opened at all.
///
/// Deliberately synchronous, and it must stay that way. Impersonation is a
/// property of the *thread*, so an `.await` between impersonating and reverting
/// would let the runtime schedule an unrelated task onto a thread still wearing
/// the client's identity.
pub fn identify(conn: &interprocess::local_socket::tokio::Stream) -> Option<PipeClient> {
	// Irrefutable here: the local-socket enum has only the named-pipe variant on
	// Windows, and only that inner type exposes the handle we need.
	let interprocess::local_socket::tokio::Stream::NamedPipe(pipe) = conn;

	// The caller must already have read the request off the pipe: impersonating
	// before the first read gets the anonymous token instead of the client's.
	let handle = pipe.as_handle().as_raw_handle() as HANDLE;

	// SAFETY: the handle is the live server end of the pipe, borrowed for no
	// longer than this call.
	if unsafe { ImpersonateNamedPipeClient(handle) } == 0 {
		return None;
	}

	let client = read_impersonated_token();

	// Unconditional: a thread left impersonating would run every later task
	// scheduled onto it as the client.
	// SAFETY: paired with the successful impersonation above.
	if unsafe { RevertToSelf() } == 0 {
		crate::warn!("could not stop impersonating an ipc client — refusing the request");
		return None;
	}

	client
}

fn read_impersonated_token() -> Option<PipeClient> {
	let mut token: HANDLE = ptr::null_mut();

	// The `TRUE` asks for the token as the impersonated client rather than as
	// ourselves, which is the only way to read who they are. It fails outright
	// for a client that connected anonymously — exactly the case that must not
	// be mistaken for a trusted one.
	// SAFETY: `token` is written only when the call reports success.
	if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } == 0 {
		return None;
	}

	let client = describe_token(token);
	// SAFETY: opened just above, and not used again.
	unsafe { CloseHandle(token) };
	client
}

fn describe_token(token: HANDLE) -> Option<PipeClient> {
	let user = token_information(token, TokenUser)?;
	// SAFETY: `TokenUser` fills the buffer with a TOKEN_USER, and `user` owns
	// that memory for the rest of this function, so `sid` stays valid.
	let sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };

	let elevation = token_information(token, TokenElevation)?;
	// SAFETY: same, for a TOKEN_ELEVATION.
	let elevated = unsafe { (*elevation.as_ptr().cast::<TOKEN_ELEVATION>()).TokenIsElevated } != 0;

	Some(PipeClient {
		sid: sid_string(sid)?,
		account: account_name(sid),
		elevated,
	})
}

/// Fetches one variable-length field out of a token.
///
/// Returned as `u64`s rather than bytes because the structures written here
/// contain pointers: reading them back out of a byte buffer would be a
/// misaligned load.
fn token_information(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Option<Vec<u64>> {
	let mut needed = 0u32;
	// The first call only asks how much room the answer needs, and is expected
	// to fail with ERROR_INSUFFICIENT_BUFFER.
	// SAFETY: a null buffer of length zero is how that question is asked.
	unsafe { GetTokenInformation(token, class, ptr::null_mut(), 0, &mut needed) };
	if needed == 0 {
		return None;
	}

	let mut buf = vec![0u64; needed.div_ceil(8) as usize];
	// SAFETY: the buffer is at least `needed` bytes and outlives the call.
	let ok = unsafe {
		GetTokenInformation(token, class, buf.as_mut_ptr().cast(), needed, &mut needed)
	};
	(ok != 0).then_some(buf)
}

fn sid_string(sid: *mut std::ffi::c_void) -> Option<String> {
	let mut raw: *mut u16 = ptr::null_mut();
	// SAFETY: `raw` receives an owned string only when this reports success.
	if unsafe { ConvertSidToStringSidW(sid, &mut raw) } == 0 {
		return None;
	}
	// SAFETY: a nul-terminated string owned by the local heap.
	let text = unsafe { wide_to_string(raw) };
	// SAFETY: allocated by ConvertSidToStringSidW, freed exactly once.
	unsafe { LocalFree(raw.cast()) };
	text
}

/// `DOMAIN\User` when the SID resolves to an account. Best-effort: this is only
/// ever shown to a human, so failing to resolve it is not failing to identify
/// the caller.
fn account_name(sid: *mut std::ffi::c_void) -> Option<String> {
	let (mut name_len, mut domain_len) = (0u32, 0u32);
	let mut kind = 0;

	// Sizing call, as above.
	// SAFETY: null buffers with zero lengths ask for the required sizes.
	unsafe {
		LookupAccountSidW(
			ptr::null(),
			sid,
			ptr::null_mut(),
			&mut name_len,
			ptr::null_mut(),
			&mut domain_len,
			&mut kind,
		)
	};
	if name_len == 0 {
		return None;
	}

	let mut name = vec![0u16; name_len as usize];
	let mut domain = vec![0u16; domain_len.max(1) as usize];
	// SAFETY: both buffers are sized by the call above and outlive this one.
	let ok = unsafe {
		LookupAccountSidW(
			ptr::null(),
			sid,
			name.as_mut_ptr(),
			&mut name_len,
			domain.as_mut_ptr(),
			&mut domain_len,
			&mut kind,
		)
	};
	if ok == 0 {
		return None;
	}

	let name = String::from_utf16_lossy(&name[..name_len as usize]);
	let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);

	Some(match domain.is_empty() {
		true => name,
		false => format!("{domain}\\{name}"),
	})
}

/// Our own identity, read the same way.
///
/// Used to check an impersonated answer against a known one, and to name the
/// account that must keep access when a file is locked down.
pub fn current_process() -> Option<PipeClient> {
	use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

	let mut token: HANDLE = ptr::null_mut();
	// SAFETY: `token` is written only on success.
	if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
		return None;
	}
	let client = describe_token(token);
	// SAFETY: opened just above, not used again.
	unsafe { CloseHandle(token) };
	client
}

/// # Safety
/// `ptr` must be null or point to a nul-terminated UTF-16 string.
unsafe fn wide_to_string(ptr: *const u16) -> Option<String> {
	if ptr.is_null() {
		return None;
	}
	let mut len = 0;
	while unsafe { *ptr.add(len) } != 0 {
		len += 1;
	}
	Some(String::from_utf16_lossy(unsafe {
		std::slice::from_raw_parts(ptr, len)
	}))
}

#[cfg(test)]
mod tests {
	use super::*;

	use interprocess::local_socket::tokio::{prelude::*, Stream};
	use interprocess::local_socket::{GenericNamespaced, ListenerOptions, ToNsName};
	use std::time::Duration;
	use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

	/// The whole claim of this module is that the identity comes from the
	/// kernel and cannot be forged, so the only honest test is a real pipe with
	/// a real client on the other end.
	#[tokio::test]
	async fn a_client_on_a_real_pipe_is_identified() {
		let name = format!("quix-winauth-{}", std::process::id());
		let listener = ListenerOptions::new()
			.name(name.clone().to_ns_name::<GenericNamespaced>().unwrap())
			.create_tokio()
			.unwrap();

		let client = tokio::spawn(async move {
			let mut conn = Stream::connect(name.to_ns_name::<GenericNamespaced>().unwrap())
				.await
				.unwrap();
			conn.write_all(b"{}\n").await.unwrap();
			// Stay connected until the server has finished looking us up.
			tokio::time::sleep(Duration::from_millis(300)).await;
		});

		let conn = listener.accept().await.unwrap();
		// Read first, exactly as the real server does: impersonating before the
		// first read yields the anonymous token rather than the client's.
		let mut line = String::new();
		BufReader::new(&conn).read_line(&mut line).await.unwrap();

		let who = identify(&conn).expect("a real client must be identifiable");
		let us = current_process().expect("our own token must be readable");

		// The client is this very process, so the kernel's answer about the
		// connection has to match the one about ourselves. If impersonation
		// silently gave us the server's identity instead, these would still
		// match — so the SID shape is checked too.
		assert_eq!(who.sid, us.sid, "identified the wrong account");
		assert_eq!(who.elevated, us.elevated, "misread elevation");
		assert!(
			who.sid.starts_with("S-1-5-21-"),
			"expected a real user account, got {}",
			who.sid
		);
		// S-1-5-7 is anonymous: reading that as a caller identity would be the
		// exact failure this module exists to avoid.
		assert_ne!(who.sid, "S-1-5-7", "anonymous must never identify a caller");

		client.await.unwrap();
	}

	#[test]
	fn an_account_name_is_resolved_for_messages() {
		let us = current_process().expect("our own token must be readable");
		let account = us.account.expect("a local account must resolve to a name");
		assert!(account.contains('\\'), "expected DOMAIN\\User, got {account}");
	}
}
