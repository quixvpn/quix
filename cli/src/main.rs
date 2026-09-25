mod commands;
#[cfg(windows)]
mod elevate;
mod service_manager;

#[tokio::main]
async fn main() {
	let Err(error) = commands::run().await else {
		return;
	};

	// A file transfer that ended by being rejected, expiring or being cancelled
	// has an exit code of its own, so a script can tell those apart from a
	// failure. Everything else reads exactly as returning the error would.
	match error.downcast_ref::<commands::file::Ended>() {
		Some(ended) => {
			eprintln!("{}", ended.message);
			std::process::exit(ended.code);
		}
		None => {
			eprintln!("Error: {error:?}");
			std::process::exit(1);
		}
	}
}