//! How offers look on a terminal and in a script.

use proto::{IncomingOffer, OfferState, OutgoingOffer};

/// `1.5 MiB`. Binary units, because that is what the file will occupy.
pub fn size(bytes: u64) -> String {
	const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
	if bytes < 1024 {
		return format!("{bytes} B");
	}
	let mut value = bytes as f64 / 1024.0;
	let mut unit = 0;
	while value >= 1024.0 && unit < UNITS.len() - 1 {
		value /= 1024.0;
		unit += 1;
	}
	format!("{value:.1} {}", UNITS[unit])
}

/// `9m 30s`: the two largest units, which is all anyone reads of a countdown.
pub fn duration(seconds: u64) -> String {
	let (h, m, s) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
	match (h, m) {
		(0, 0) => format!("{s}s"),
		(0, _) => format!("{m}m {s}s"),
		_ => format!("{h}h {m}m"),
	}
}

/// A transfer's progress, redrawn in place on stderr — and only when stderr is
/// a terminal, so a script capturing output gets the result and nothing else.
pub struct Progress {
	total: u64,
	shown: Option<std::time::Instant>,
	enabled: bool,
}

impl Progress {
	pub fn new(total: u64) -> Self {
		use std::io::IsTerminal;
		Self {
			total,
			shown: None,
			enabled: std::io::stderr().is_terminal(),
		}
	}

	pub fn update(&mut self, done: u64) {
		// A few redraws a second is plenty, and a terminal is slow to scroll.
		let due = self
			.shown
			.is_none_or(|at| at.elapsed() >= std::time::Duration::from_millis(100));
		if self.enabled && due {
			self.shown = Some(std::time::Instant::now());
			let percent = done
				.saturating_mul(100)
				.checked_div(self.total)
				.unwrap_or(100);
			eprint!("\r\x1b[K{} / {}  {percent}%", size(done), size(self.total));
		}
	}

	/// Clears the line, so whatever is printed next starts clean.
	pub fn done(&mut self) {
		if self.enabled && self.shown.is_some() {
			eprint!("\r\x1b[K");
		}
	}
}

/// How long a transfer took: `40ms` under a second, `3.2s` under a minute,
/// where the tenths still matter, and like [`duration`] past it.
pub fn elapsed(took: std::time::Duration) -> String {
	match took.as_secs() {
		0 => format!("{}ms", took.as_millis()),
		1..60 => format!("{:.1}s", took.as_secs_f64()),
		secs => duration(secs),
	}
}

/// The time left on an offer, redrawn in place on stderr every second under
/// the same terms as [`Progress`]: only at a terminal.
pub struct Countdown {
	deadline: std::time::Instant,
	shown: bool,
	enabled: bool,
}

impl Countdown {
	pub fn new(seconds: u64) -> Self {
		use std::io::IsTerminal;
		Self {
			deadline: std::time::Instant::now() + std::time::Duration::from_secs(seconds),
			shown: false,
			enabled: std::io::stderr().is_terminal(),
		}
	}

	pub fn enabled(&self) -> bool {
		self.enabled
	}

	pub fn show(&mut self) {
		if !self.enabled {
			return;
		}
		self.shown = true;
		// Rounded up, so it starts at the full window and reads 0s only once
		// the offer has actually run out.
		let left = self
			.deadline
			.saturating_duration_since(std::time::Instant::now());
		let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
		eprint!("\r\x1b[Kexpires in {}", duration(secs));
	}

	/// Clears the line, so whatever is printed next starts clean.
	pub fn done(&mut self) {
		if self.enabled && self.shown {
			eprint!("\r\x1b[K");
		}
	}
}

/// One line per incoming offer, for the selector.
pub fn incoming_line(offer: &IncomingOffer) -> String {
	format!(
		"{}  {}  {}  {}  expires in {}",
		offer.id,
		offer.from,
		offer.name,
		size(offer.size),
		duration(offer.expires_in_secs)
	)
}

/// A plain table of offers waiting here, for when nobody is at a terminal.
/// Tab-separated with a header, so `cut` and `awk` can take it apart.
pub fn incoming_table(offers: &[IncomingOffer]) -> String {
	let mut table = String::from("ID\tFROM\tNAME\tSIZE\tEXPIRES\n");
	for offer in offers {
		table.push_str(&format!(
			"{}\t{}\t{}\t{}\t{}\n",
			offer.id,
			offer.from,
			offer.name,
			size(offer.size),
			duration(offer.expires_in_secs)
		));
	}
	table
}

/// This node's own offers and where each stands.
pub fn outgoing_table(offers: &[OutgoingOffer]) -> String {
	let mut table = String::from("ID\tTO\tNAME\tSIZE\tSTATE\n");
	for offer in offers {
		table.push_str(&format!(
			"{}\t{}\t{}\t{}\t{}\n",
			offer.id,
			offer.to,
			offer.name,
			size(offer.size),
			state(offer)
		));
	}
	table
}

fn state(offer: &OutgoingOffer) -> String {
	match (offer.state, offer.expires_in_secs, &offer.detail) {
		(OfferState::Offered, Some(left), _) => format!("offered, expires in {}", duration(left)),
		(state, _, Some(detail)) => format!("{} ({detail})", state.label()),
		(state, _, None) => state.label().to_string(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn incoming(id: &str, name: &str, size: u64, expires_in_secs: u64) -> IncomingOffer {
		IncomingOffer {
			id: id.to_string(),
			from: "nas".to_string(),
			name: name.to_string(),
			size,
			expires_in_secs,
		}
	}

	#[test]
	fn sizes_read_naturally() {
		assert_eq!(size(0), "0 B");
		assert_eq!(size(1023), "1023 B");
		assert_eq!(size(1024), "1.0 KiB");
		assert_eq!(size(1536), "1.5 KiB");
		assert_eq!(size(20 * 1024 * 1024 * 1024), "20.0 GiB");
		assert_eq!(size(u64::MAX), "16384.0 PiB");
	}

	#[test]
	fn countdowns_show_the_two_largest_units() {
		assert_eq!(duration(45), "45s");
		assert_eq!(duration(570), "9m 30s");
		assert_eq!(duration(7500), "2h 5m");
	}

	#[test]
	fn elapsed_keeps_tenths_under_a_minute() {
		use std::time::Duration;
		assert_eq!(elapsed(Duration::from_millis(40)), "40ms");
		assert_eq!(elapsed(Duration::from_millis(3240)), "3.2s");
		assert_eq!(elapsed(Duration::from_millis(59_900)), "59.9s");
		assert_eq!(elapsed(Duration::from_secs(95)), "1m 35s");
	}

	#[test]
	fn the_plain_table_has_a_header_and_one_row_per_offer() {
		let table = incoming_table(&[
			incoming("0000aaaa", "photo.jpg", 2048, 570),
			incoming("0000bbbb", "notes.txt", 12, 45),
		]);
		let lines: Vec<&str> = table.lines().collect();
		assert_eq!(lines[0], "ID\tFROM\tNAME\tSIZE\tEXPIRES");
		assert_eq!(lines[1], "0000aaaa\tnas\tphoto.jpg\t2.0 KiB\t9m 30s");
		assert_eq!(lines[2], "0000bbbb\tnas\tnotes.txt\t12 B\t45s");
		assert_eq!(lines.len(), 3);
	}

	#[test]
	fn an_empty_table_is_just_its_header() {
		assert_eq!(incoming_table(&[]), "ID\tFROM\tNAME\tSIZE\tEXPIRES\n");
	}

	#[test]
	fn outgoing_offers_show_their_state() {
		let offer = |state, expires_in_secs, detail: Option<&str>| OutgoingOffer {
			id: "0000cccc".to_string(),
			to: "laptop".to_string(),
			name: "a.iso".to_string(),
			size: 10,
			state,
			expires_in_secs,
			detail: detail.map(str::to_string),
		};
		let table = outgoing_table(&[
			offer(OfferState::Offered, Some(300), None),
			offer(OfferState::Transferring, None, None),
			offer(OfferState::Done, None, None),
			offer(OfferState::Failed, None, Some("the receiver cancelled")),
		]);
		let rows: Vec<&str> = table
			.lines()
			.skip(1)
			.map(|l| l.rsplit('\t').next().unwrap())
			.collect();
		assert_eq!(
			rows,
			[
				"offered, expires in 5m 0s",
				"transferring",
				"done",
				"failed (the receiver cancelled)"
			]
		);
	}

	#[test]
	fn a_selector_line_has_every_field() {
		let line = incoming_line(&incoming("0000aaaa", "photo.jpg", 2048, 570));
		for field in ["0000aaaa", "nas", "photo.jpg", "2.0 KiB", "9m 30s"] {
			assert!(line.contains(field), "{line}");
		}
	}
}
