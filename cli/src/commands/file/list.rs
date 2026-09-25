use std::io::IsTerminal;

use anyhow::Result;
use clap::Args;
use dialoguer::{Confirm, Select};
use proto::IncomingOffer;

use super::{dest, format, receive, Ended};

/// Show offers waiting here, and pick one to accept or reject
///
/// At a terminal, pick an offer with the arrow keys and answer Y or n; Esc or q
/// leaves. Anywhere else, prints a plain table and exits, for scripts —
/// `quix file accept` and `quix file reject` take an id without asking.
#[derive(Args)]
pub struct ListArgs {
	/// Save accepted files into the current directory instead of Downloads
	#[arg(long)]
	pub here: bool,
}

pub async fn run(args: ListArgs) -> Result<()> {
	// Resolved now, while the working directory is the one the user ran this
	// from, and before anything is shown that they could act on.
	let dest = dest::for_caller(args.here)?;
	let (incoming, outgoing) = receive::offers().await?;

	let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
	if !interactive {
		print!("{}", format::incoming_table(&incoming));
		if !outgoing.is_empty() {
			println!();
			print!("{}", format::outgoing_table(&outgoing));
		}
		return Ok(());
	}

	// This node's own offers are shown, not acted on.
	if !outgoing.is_empty() {
		println!("Sent:");
		for line in format::outgoing_table(&outgoing).lines().skip(1) {
			println!("  {}", line.replace('\t', "  "));
		}
		println!();
	}

	let mut incoming = incoming;
	loop {
		if incoming.is_empty() {
			println!("No offers waiting.");
			return Ok(());
		}

		let offer = match pick(&incoming)? {
			Some(offer) => offer,
			None => return Ok(()),
		};

		let question = format!(
			"Accept {} ({}) from {}?",
			offer.name,
			format::size(offer.size),
			offer.from
		);
		// Y is the default, so Enter accepts. Esc or q goes back to the list, or
		// out when there was only the one offer to choose from.
		match Confirm::new()
			.with_prompt(question)
			.default(true)
			.interact_opt()?
		{
			Some(true) => {
				if let Some(note) = &dest.note {
					println!("{note}");
				}
				match receive::accept(&offer.id, &dest.dir).await {
					Ok(saved) => println!("saved {}", saved.display()),
					// Ctrl+C mid-transfer stops everything, not just this one.
					Err(e) if e.is::<Ended>() => return Err(e),
					Err(e) => eprintln!("{e:#}"),
				}
			}
			Some(false) => match receive::reject(&offer.id).await {
				Ok((name, from)) => println!("rejected {name} from {from}"),
				Err(e) => eprintln!("{e:#}"),
			},
			None if incoming.len() == 1 => return Ok(()),
			None => {}
		}

		incoming = receive::offers().await?.0;
	}
}

/// Asks which offer to act on. With only one there is nothing to choose, so it
/// is simply the one.
fn pick(incoming: &[IncomingOffer]) -> Result<Option<IncomingOffer>> {
	if let [only] = incoming {
		return Ok(Some(only.clone()));
	}
	let lines: Vec<String> = incoming.iter().map(format::incoming_line).collect();
	let chosen = Select::new()
		.with_prompt("Offers waiting (Esc or q to leave)")
		.items(&lines)
		.default(0)
		.interact_opt()?;
	Ok(chosen.map(|i| incoming[i].clone()))
}
