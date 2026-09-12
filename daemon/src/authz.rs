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
#[derive(Debug, Clone)]
pub enum Caller {
	/// Peer's effective uid, from SO_PEERCRED. Unix only.
	#[cfg_attr(not(unix), allow(dead_code))]
	Uid(u32),
	/// The client's user SID and whether its token is elevated, read by
	/// impersonating the pipe client. Windows only.
	Windows {
		sid: String,
		account: Option<String>,
		elevated: bool,
	},
	/// The platform gave us no usable identity. Treated as unprivileged.
	Unknown,
}

impl Caller {
	/// Reads the peer's credentials off a live connection. This is the kernel's
	/// answer, not something the client can claim, so it cannot be forged.
	///
	/// Must be called only after the request has been read off the connection:
	/// on Windows, impersonating before the first read yields the anonymous
	/// token rather than the client's.
	pub fn of(conn: &interprocess::local_socket::tokio::Stream) -> Self {
		#[cfg(unix)]
		{
			use interprocess::local_socket::traits::StreamCommon as _;
			match conn.peer_creds().ok().and_then(|creds| creds.euid()) {
				Some(uid) => Caller::Uid(uid),
				None => Caller::Unknown,
			}
		}
		#[cfg(windows)]
		{
			match crate::winauth::identify(conn) {
				Some(client) => Caller::Windows {
					sid: client.sid,
					account: client.account,
					elevated: client.elevated,
				},
				None => Caller::Unknown,
			}
		}
		#[cfg(not(any(unix, windows)))]
		{
			let _ = conn;
			Caller::Unknown
		}
	}

	/// Whether this caller may do anything, without consulting the operator.
	///
	/// Elevation is the closest Windows analogue of uid 0: it is what the SCM
	/// and the registry already demand of anything that administers the daemon.
	fn is_superuser(&self) -> bool {
		match self {
			Caller::Uid(uid) => *uid == 0,
			Caller::Windows { elevated, .. } => *elevated,
			Caller::Unknown => false,
		}
	}

	/// Whether this caller is the account allowed to mutate without being one.
	///
	/// Each platform matches on its own kind of identity and never on the
	/// other's, so an operator recorded on one cannot be mistaken for a caller
	/// on the other. An unset operator matches nobody.
	fn is_operator(&self, operator: &Operator) -> bool {
		match self {
			Caller::Uid(uid) => operator.uid == Some(*uid),
			// Both sides produce the canonical string form — the daemon via
			// ConvertSidToStringSidW, the installer via .NET's
			// SecurityIdentifier — so comparing them is comparing the SIDs.
			Caller::Windows { sid, .. } => operator.sid.as_deref() == Some(sid.as_str()),
			Caller::Unknown => false,
		}
	}

	pub fn describe(&self) -> String {
		match self {
			Caller::Uid(uid) => match user_name(*uid) {
				Some(name) => format!("{name} (uid {uid})"),
				None => format!("uid {uid}"),
			},
			Caller::Windows { sid, account, .. } => match account {
				Some(account) => format!("{account} ({sid})"),
				None => sid.clone(),
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
		| Request::Invite { .. }
		| Request::Join { .. }
		| Request::Leave
		| Request::SetHostname { .. }
		| Request::SetOperator { .. } => false,
	}
}

/// The one account, per platform, that may mutate without being a superuser.
///
/// Set by the installer to whoever ran it, so day-to-day commands do not need
/// `sudo` on Unix or a UAC prompt on Windows. Both are stored; only the one
/// matching the caller's platform can ever match.
#[derive(Debug, Clone, Default)]
pub struct Operator {
	/// Unix: the uid allowed to mutate.
	pub uid: Option<u32>,
	/// Windows: the user SID allowed to mutate.
	pub sid: Option<String>,
}

/// Returns an error message when the caller may not run this command.
pub fn check(req: &Request, caller: &Caller, operator: &Operator) -> Result<(), String> {
	if is_read_only(req) || caller.is_superuser() || caller.is_operator(operator) {
		return Ok(());
	}

	Err(denial(caller))
}

/// Says how to get past a refusal, in the terms of the platform being refused on.
fn denial(caller: &Caller) -> String {
	let who = caller.describe();

	match cfg!(windows) {
		// The CLI retries elevated when it sees this, so a human normally gets a
		// UAC prompt rather than the message. Anything else talking to the pipe
		// is told what it is missing.
		true => format!(
			"{who} is not authorized — run elevated, or reinstall to make this \
			 account the operator"
		),
		false => format!(
			"{who} is not authorized — run as root, or have the operator set with \
			 `sudo quix set-operator <user>`"
		),
	}
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

/// There is no operator on Windows.
///
/// The operator is a uid, and a uid means nothing here. Accepting a number
/// anyway would store an identity that can never match a caller, which reads as
/// "it worked" and grants nothing — worse than saying so.
#[cfg(not(unix))]
pub fn resolve_user(user: &str) -> Result<u32, String> {
	Err(format!(
		"set-operator is not supported on Windows: authorization here is by \
		 Administrator elevation, so run the command elevated instead (got {user:?})"
	))
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
	const OPERATOR_SID: &str = "S-1-5-21-1-2-3-1001";

	fn nobody() -> Operator {
		Operator::default()
	}

	fn operator_uid(uid: u32) -> Operator {
		Operator {
			uid: Some(uid),
			sid: None,
		}
	}

	fn operator_sid(sid: &str) -> Operator {
		Operator {
			uid: None,
			sid: Some(sid.to_string()),
		}
	}

	fn status() -> Request {
		Request::Status
	}

	fn mutating() -> Request {
		Request::CreateNetwork {
			name: "net".to_string(),
			hostname: None,
		}
	}

	fn windows(elevated: bool) -> Caller {
		Caller::Windows {
			sid: "S-1-5-21-1-2-3-1001".to_string(),
			account: Some(r"DESKTOP\user".to_string()),
			elevated,
		}
	}

	#[test]
	fn anyone_may_read_state() {
		assert!(check(&status(), &Caller::Uid(4242), &nobody()).is_ok());
		assert!(check(&status(), &Caller::Unknown, &nobody()).is_ok());
		assert!(check(&status(), &windows(false), &nobody()).is_ok());
	}

	#[test]
	fn an_unidentified_caller_may_never_mutate() {
		// The Windows hole this closes. `Caller::Unknown` used to be waved
		// through on any non-Unix platform, so any local process at all could
		// open the pipe and issue Join, Leave or Invite without being anyone in
		// particular. Unknown means unprivileged, on every platform.
		assert!(check(&mutating(), &Caller::Unknown, &nobody()).is_err());
		assert!(check(&mutating(), &Caller::Unknown, &operator_uid(OPERATOR)).is_err());
	}

	#[test]
	fn an_elevated_windows_caller_may_mutate() {
		// Elevation is the Windows stand-in for uid 0.
		assert!(check(&mutating(), &windows(true), &nobody()).is_ok());
	}

	#[test]
	fn an_unelevated_windows_caller_may_not_mutate() {
		let denied = check(&mutating(), &windows(false), &nobody());
		assert!(denied.is_err(), "an ordinary user must not change membership");
		assert!(
			denied.unwrap_err().contains(r"DESKTOP\user"),
			"names who was refused"
		);
	}

	#[test]
	fn the_operator_is_not_a_way_around_elevation_on_windows() {
		// The operator is a uid. A Windows caller can never match one, so a set
		// operator must not soften the elevation requirement.
		assert!(check(&mutating(), &windows(false), &operator_uid(OPERATOR)).is_err());
	}

	#[test]
	fn a_refusal_says_how_to_get_past_it_on_this_platform() {
		let message = check(&mutating(), &Caller::Unknown, &nobody()).unwrap_err();
		match cfg!(windows) {
			// Both routes out, since either may be the one available.
			true => assert!(
				message.contains("elevated") && message.contains("operator"),
				"{message}"
			),
			false => assert!(message.contains("set-operator"), "{message}"),
		}
	}

	#[test]
	fn the_windows_operator_may_mutate_without_being_elevated() {
		// The whole point: the account that installed quix runs day-to-day
		// commands without a UAC prompt, exactly as the Unix operator does.
		let caller = windows(false);
		assert!(check(&mutating(), &caller, &operator_sid(OPERATOR_SID)).is_ok());
	}

	#[test]
	fn another_windows_account_is_not_the_operator() {
		let caller = windows(false);
		assert!(check(&mutating(), &caller, &operator_sid("S-1-5-21-9-9-9-500")).is_err());
	}

	#[test]
	fn an_operator_from_the_other_platform_grants_nothing() {
		// A settings file carried between platforms, or written by hand, must
		// not have a uid stand in for a SID or the reverse.
		assert!(check(&mutating(), &windows(false), &operator_uid(OPERATOR)).is_err());
		assert!(check(&mutating(), &Caller::Uid(OPERATOR), &operator_sid(OPERATOR_SID)).is_err());
	}

	#[test]
	fn an_unset_operator_matches_nobody() {
		// The failure that would matter most: `None == None` reading as a match
		// and handing every unidentified caller the operator's rights.
		assert!(check(&mutating(), &Caller::Unknown, &nobody()).is_err());
		assert!(check(&mutating(), &windows(false), &nobody()).is_err());
		assert!(check(&mutating(), &Caller::Uid(OPERATOR), &nobody()).is_err());
	}

	#[test]
	fn a_sid_is_matched_exactly_and_not_by_prefix() {
		// S-1-5-21-1-2-3-1001 and S-1-5-21-1-2-3-10011 are different accounts.
		let longer = format!("{OPERATOR_SID}1");
		assert!(check(&mutating(), &windows(false), &operator_sid(&longer)).is_err());
	}

	#[test]
	fn root_may_do_anything() {
		assert!(check(&mutating(), &Caller::Uid(0), &nobody()).is_ok());
	}

	#[test]
	fn the_operator_may_mutate() {
		assert!(check(&mutating(), &Caller::Uid(OPERATOR), &operator_uid(OPERATOR)).is_ok());
	}

	#[test]
	fn another_user_may_not_mutate() {
		let denied = check(&mutating(), &Caller::Uid(4242), &operator_uid(OPERATOR));
		assert!(denied.is_err(), "a non-operator must not change membership");
		// How to get past it is worded per platform, and checked separately by
		// `a_refusal_says_how_to_get_past_it_on_this_platform`.
		assert!(denied.unwrap_err().contains("not authorized"));
	}

	#[test]
	fn with_no_operator_set_only_root_may_mutate() {
		assert!(check(&mutating(), &Caller::Uid(OPERATOR), &nobody()).is_err());
	}

	#[cfg(unix)]
	#[test]
	fn numeric_uids_resolve_without_a_passwd_entry() {
		assert_eq!(resolve_user("1234"), Ok(1234));
	}

	#[cfg(not(unix))]
	#[test]
	fn setting_an_operator_is_refused_rather_than_silently_useless() {
		// A stored uid could never match a Windows caller, so accepting one
		// would look like it granted something and grant nothing.
		let refused = resolve_user("1234").unwrap_err();
		assert!(refused.contains("not supported on Windows"), "{refused}");
	}
}
