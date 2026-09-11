//! Authorizes IPC commands by the caller's identity, the way Tailscale does,
//! rather than by the socket's file permissions.
//!
//! A permission bit on the socket is all-or-nothing: it cannot distinguish
//! reading `quix status` from making the machine leave a network. A Unix group
//! has the same problem, plus it needs a fresh login session to take effect and
//! has no Windows equivalent. Reading the peer's credentials off the connection
//! gives a read-only/mutating split and one model for both platforms.

use proto::Request;

/// Who is on the other end of an IPC connection.
#[derive(Debug, Clone, Copy)]
pub enum Caller {
	/// Peer's effective uid, from SO_PEERCRED. Never constructed on Windows,
	/// which has no identity for us to read yet.
	#[cfg_attr(not(unix), allow(dead_code))]
	Uid(u32),
	/// The platform gave us no usable identity. Treated as unprivileged.
	Unknown,
}

impl Caller {
	/// Reads the peer's credentials off a live connection. This is the kernel's
	/// answer, not something the client can claim, so it cannot be forged.
	pub fn of(conn: &interprocess::local_socket::tokio::Stream) -> Self {
		#[cfg(unix)]
		{
			use interprocess::local_socket::traits::StreamCommon as _;
			match conn.peer_creds().ok().and_then(|creds| creds.euid()) {
				Some(uid) => Caller::Uid(uid),
				None => Caller::Unknown,
			}
		}
		// TODO(windows): the pipe gives us the client's PID; turning that into a
		// user SID needs OpenProcessToken + GetTokenInformation. Until then no
		// caller is identified, so authorization is not enforced — see README.
		#[cfg(not(unix))]
		{
			let _ = conn;
			Caller::Unknown
		}
	}

	fn is_superuser(&self) -> bool {
		matches!(self, Caller::Uid(0))
	}

	pub fn describe(&self) -> String {
		match self {
			Caller::Uid(uid) => match user_name(*uid) {
				Some(name) => format!("{name} (uid {uid})"),
				None => format!("uid {uid}"),
			},
			Caller::Unknown => "unidentified caller".to_string(),
		}
	}
}

/// Commands that only read state are open to any local user; everything else
/// changes what this machine belongs to and needs authorization.
fn is_read_only(req: &Request) -> bool {
	match req {
		Request::Status | Request::Ping { .. } => true,
		Request::CreateNetwork { .. }
		| Request::Invite
		| Request::Join { .. }
		| Request::Leave
		| Request::SetOperator { .. } => false,
	}
}

/// Returns an error message when the caller may not run this command.
pub fn check(req: &Request, caller: &Caller, operator: Option<u32>) -> Result<(), String> {
	if is_read_only(req) || caller.is_superuser() {
		return Ok(());
	}

	if let (Caller::Uid(uid), Some(operator)) = (caller, operator) {
		if *uid == operator {
			return Ok(());
		}
	}

	// Windows has no identity to check yet, so refusing here would lock out
	// every command on that platform rather than securing anything.
	if matches!(caller, Caller::Unknown) && cfg!(not(unix)) {
		return Ok(());
	}

	Err(format!(
		"{} is not authorized — run as root, or have the operator set with \
		 `sudo quix set-operator <user>`",
		caller.describe()
	))
}

/// Resolves a username (or a numeric uid) to a uid.
#[cfg(unix)]
pub fn resolve_user(user: &str) -> Result<u32, String> {
	if let Ok(uid) = user.parse::<u32>() {
		return Ok(uid);
	}
	uzers::get_user_by_name(user)
		.map(|u| u.uid())
		.ok_or_else(|| format!("no such user: {user}"))
}

#[cfg(not(unix))]
pub fn resolve_user(user: &str) -> Result<u32, String> {
	user.parse::<u32>()
		.map_err(|_| format!("setting an operator by name is not supported on this platform: {user}"))
}

#[cfg(unix)]
fn user_name(uid: u32) -> Option<String> {
	uzers::get_user_by_uid(uid).map(|u| u.name().to_string_lossy().into_owned())
}

#[cfg(not(unix))]
fn user_name(_uid: u32) -> Option<String> {
	None
}

#[cfg(test)]
mod tests {
	use super::*;

	const OPERATOR: u32 = 1000;

	fn status() -> Request {
		Request::Status
	}

	fn mutating() -> Request {
		Request::CreateNetwork {
			name: "net".to_string(),
		}
	}

	#[test]
	fn anyone_may_read_state() {
		assert!(check(&status(), &Caller::Uid(4242), None).is_ok());
		assert!(check(&status(), &Caller::Unknown, None).is_ok());
	}

	#[test]
	fn root_may_do_anything() {
		assert!(check(&mutating(), &Caller::Uid(0), None).is_ok());
	}

	#[test]
	fn the_operator_may_mutate() {
		assert!(check(&mutating(), &Caller::Uid(OPERATOR), Some(OPERATOR)).is_ok());
	}

	#[test]
	fn another_user_may_not_mutate() {
		let denied = check(&mutating(), &Caller::Uid(4242), Some(OPERATOR));
		assert!(denied.is_err(), "a non-operator must not change membership");
		assert!(denied.unwrap_err().contains("set-operator"), "says how to fix it");
	}

	#[test]
	fn with_no_operator_set_only_root_may_mutate() {
		assert!(check(&mutating(), &Caller::Uid(OPERATOR), None).is_err());
	}

	#[cfg(unix)]
	#[test]
	fn an_unidentified_caller_is_refused_on_unix() {
		assert!(check(&mutating(), &Caller::Unknown, Some(OPERATOR)).is_err());
	}

	#[test]
	fn numeric_uids_resolve_without_a_passwd_entry() {
		assert_eq!(resolve_user("1234"), Ok(1234));
	}
}
