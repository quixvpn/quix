//! `--expires <amount> <unit>`, shared by every command that gives something a
//! window: invites and file offers take the same words and mean the same thing
//! by them, each with its own ceiling.

/// Turns `30 min` into seconds, refusing zero and anything past `max`.
///
/// `what` names the thing being given the window, for the refusals: "an
/// invite", "a file offer".
///
/// Both spellings of each unit are taken: someone typing `--expires 1 days` or
/// `--expires 30 minutes` means the obvious thing, and refusing it would be
/// pedantry rather than safety.
pub fn parse(amount: &str, unit: &str, max: u64, what: &str) -> Result<u64, String> {
	let amount: u64 = amount
		.parse()
		.map_err(|_| format!("{amount:?} is not a whole number of {unit}"))?;

	if amount == 0 {
		return Err(format!(
			"{what} that expires immediately could never be used"
		));
	}

	let per_unit = match unit.to_ascii_lowercase().as_str() {
		"min" | "mins" | "minute" | "minutes" => 60,
		"hour" | "hours" | "hr" | "hrs" => 60 * 60,
		"day" | "days" => 24 * 60 * 60,
		other => return Err(format!("unknown unit {other:?} — use min, hours or days")),
	};

	// Checked, or a large enough amount wraps into a short window — the one
	// arithmetic mistake here that would weaken rather than break things.
	amount
		.checked_mul(per_unit)
		.filter(|s| *s <= max)
		.ok_or_else(|| format!("the longest {what} may last is {}", span(max)))
}

/// A window in the largest whole unit that fits it: `1 day`, `90 minutes`.
pub fn span(seconds: u64) -> String {
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
		1 => format!("1 {unit}"),
		n => format!("{n} {unit}s"),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const DAY: u64 = 24 * 60 * 60;

	#[test]
	fn the_ceiling_is_the_callers() {
		assert_eq!(parse("24", "hours", DAY, "a file offer"), Ok(DAY));
		let refused = parse("25", "hours", DAY, "a file offer").unwrap_err();
		assert!(
			refused.contains("a file offer") && refused.contains("1 day"),
			"{refused}"
		);
		assert_eq!(
			parse("25", "hours", 30 * DAY, "an invite"),
			Ok(25 * 60 * 60)
		);
	}

	#[test]
	fn a_refusal_names_what_was_being_given_the_window() {
		assert!(parse("0", "min", DAY, "a file offer")
			.unwrap_err()
			.contains("a file offer"));
	}

	#[test]
	fn spans_read_the_way_people_say_them() {
		assert_eq!(span(DAY), "1 day");
		assert_eq!(span(30 * DAY), "30 days");
		assert_eq!(span(90 * 60), "90 minutes");
		assert_eq!(span(45), "45 seconds");
	}
}
