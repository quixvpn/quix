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

/// Turns `30 min` into seconds.
///
/// Both spellings of each unit are taken: someone typing `--expires 1 days` or
/// `--expires 30 minutes` means the obvious thing, and refusing it would be
/// pedantry rather than safety.
fn parse_expiry(amount: &str, unit: &str) -> Result<u64, String> {
	let amount: u64 = amount
		.parse()
		.map_err(|_| format!("{amount:?} is not a whole number of {unit}"))?;

	if amount == 0 {
		return Err("an invite that expires immediately cannot be redeemed".to_string());
	}

	let per_unit = match unit.to_ascii_lowercase().as_str() {
		"min" | "mins" | "minute" | "minutes" => 60,
		"hour" | "hours" | "hr" | "hrs" => 60 * 60,
		"day" | "days" => 24 * 60 * 60,
		other => return Err(format!("unknown unit {other:?} — use min, hours or days")),
	};

	// Checked, or a large enough amount wraps into a short window — the one
	// arithmetic mistake here that would weaken rather than break things.
	let seconds = amount
		.checked_mul(per_unit)
		.filter(|s| *s <= MAX_INVITE_TTL)
		.ok_or_else(|| {
			format!(
				"the longest an invite may last is {} days",
				MAX_INVITE_TTL / (24 * 60 * 60)
			)
		})?;

	Ok(seconds)
}

/// The window in the words it was asked for, rather than a pile of seconds.
fn describe(seconds: u64) -> String {
	const MINUTE: u64 = 60;
	const HOUR: u64 = 60 * MINUTE;
	const DAY: u64 = 24 * HOUR;

	let (count, unit) = match seconds {
		s if s % DAY == 0 && s >= DAY => (s / DAY, "day"),
		s if s % HOUR == 0 && s >= HOUR => (s / HOUR, "hour"),
		s if s % MINUTE == 0 && s >= MINUTE => (s / MINUTE, "minute"),
		s => (s, "second"),
	};

	match count {
		1 => format!("in 1 {unit}"),
		n => format!("in {n} {unit}s"),
	}
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
