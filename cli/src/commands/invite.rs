use anyhow::Result;
use clap::Args;
use proto::{Request, Response, DEFAULT_INVITE_TTL, MAX_INVITE_TTL};

use super::client::send;

/// Generate a one-time invite code (coordinator only)
#[derive(Args)]
pub struct InviteArgs {
	/// How long the code stays valid, e.g. `--expires 30 min`, `--expires 2 days`
	///
	/// Units: min, hours, days. Defaults to 5 minutes.
	#[arg(long, num_args = 2, value_names = ["AMOUNT", "UNIT"])]
	pub expires: Option<Vec<String>>,
}

/// The window these arguments ask for, in seconds.
///
/// Separate from `run` so it can be checked before anything irreversible or
/// interactive happens — on Windows `invite` asks for elevation, and a UAC
/// prompt for a command that is about to reject its own arguments is a poor
/// trade for the user.
pub fn ttl_secs(args: &InviteArgs) -> Result<u64, String> {
	match &args.expires {
		Some(parts) => parse_expiry(&parts[0], &parts[1]),
		None => Ok(DEFAULT_INVITE_TTL),
	}
}

pub async fn run(args: InviteArgs) -> Result<()> {
	let ttl_secs = ttl_secs(&args).map_err(|e| anyhow::anyhow!(e))?;

	match send(Request::Invite { ttl_secs }).await? {
		Response::Invite { code, expires_at } => {
			println!("invite code: {code}");
			println!("expires:     {} ({})", describe(ttl_secs), expires_at);
			println!("single use — redeeming it consumes it, whatever time is left");
			Ok(())
		}
		Response::Error { message } => anyhow::bail!("invite failed: {message}"),
		_ => anyhow::bail!("unexpected response"),
	}
}

/// Turns `30 min` into seconds, within what an invite may last.
fn parse_expiry(amount: &str, unit: &str) -> Result<u64, String> {
	super::expiry::parse(amount, unit, MAX_INVITE_TTL, "an invite")
}

/// The window in the words it was asked for, rather than a pile of seconds.
fn describe(seconds: u64) -> String {
	format!("in {}", super::expiry::span(seconds))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_documented_forms_parse() {
		assert_eq!(parse_expiry("30", "min"), Ok(30 * 60));
		assert_eq!(parse_expiry("2", "days"), Ok(2 * 24 * 60 * 60));
		assert_eq!(parse_expiry("1", "hours"), Ok(60 * 60));
	}

	#[test]
	fn either_spelling_of_a_unit_works() {
		assert_eq!(parse_expiry("5", "min"), parse_expiry("5", "minutes"));
		assert_eq!(parse_expiry("1", "day"), parse_expiry("1", "days"));
		assert_eq!(parse_expiry("3", "hr"), parse_expiry("3", "hours"));
		// Typed however the shell left it.
		assert_eq!(parse_expiry("5", "MIN"), Ok(5 * 60));
	}

	#[test]
	fn nonsense_is_refused_with_a_reason() {
		assert!(parse_expiry("soon", "min").is_err());
		assert!(parse_expiry("5", "fortnights").unwrap_err().contains("min, hours or days"));
		assert!(parse_expiry("-1", "min").is_err(), "negative is not a duration");
	}

	#[test]
	fn a_zero_window_is_refused_rather_than_minted_dead() {
		// Handing someone a code that can never work is worse than saying no.
		assert!(parse_expiry("0", "min").is_err());
	}

	#[test]
	fn an_absurd_window_is_refused_instead_of_wrapping() {
		// u64 seconds overflow long before the year does, and a wrap would turn
		// "forever" into "a few seconds" — or worse, something that looks sane.
		assert!(parse_expiry(&u64::MAX.to_string(), "days").is_err());
		assert!(parse_expiry("3650", "days").is_err(), "past the cap");
		assert_eq!(parse_expiry("30", "days"), Ok(MAX_INVITE_TTL), "at the cap");
	}

	#[test]
	fn the_default_is_five_minutes() {
		assert_eq!(DEFAULT_INVITE_TTL, 5 * 60);
		assert_eq!(describe(DEFAULT_INVITE_TTL), "in 5 minutes");
	}

	#[test]
	fn a_window_reads_back_the_way_it_was_asked_for() {
		assert_eq!(describe(parse_expiry("30", "min").unwrap()), "in 30 minutes");
		assert_eq!(describe(parse_expiry("2", "days").unwrap()), "in 2 days");
		assert_eq!(describe(parse_expiry("1", "hours").unwrap()), "in 1 hour");
	}
}
