//! Resolves the version the binaries report, so a release is just a git tag.
//!
//! In order of preference:
//!   1. `QUIX_VERSION`, which CI sets from the tag being built.
//!   2. `git describe`, so a local build says which commit it came from —
//!      `0.1.3-5-gabc1234-dirty` is far more useful than a stale `0.1.0`.
//!   3. The `Cargo.toml` version, for building from a source tarball with no
//!      git metadata and no CI.

use std::process::Command;

fn main() {
	println!("cargo:rerun-if-env-changed=QUIX_VERSION");
	// Pick up new commits and tags without a clean rebuild.
	for path in ["../.git/HEAD", "../.git/refs/tags"] {
		if std::path::Path::new(path).exists() {
			println!("cargo:rerun-if-changed={path}");
		}
	}

	let version = from_env()
		.or_else(from_git)
		.or_else(|| std::env::var("CARGO_PKG_VERSION").ok())
		.unwrap_or_else(|| "unknown".to_string());

	// Both forms, because `concat!` only takes literals and `cargo:rustc-env`
	// only reaches this crate — downstream crates can't build the tagged form
	// themselves at compile time.
	println!("cargo:rustc-env=QUIX_VERSION={version}");
	println!("cargo:rustc-env=QUIX_VERSION_TAG=v{version}");
}

/// Tags are written `v0.1.3`; the leading `v` is added back when displayed, so
/// strip it here and keep one representation everywhere.
fn normalize(raw: &str) -> Option<String> {
	let trimmed = raw.trim().trim_start_matches('v');
	(!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn from_env() -> Option<String> {
	normalize(&std::env::var("QUIX_VERSION").ok()?)
}

fn from_git() -> Option<String> {
	let output = Command::new("git")
		.args(["describe", "--tags", "--always", "--dirty"])
		.output()
		.ok()?;

	output
		.status
		.success()
		.then(|| normalize(&String::from_utf8(output.stdout).ok()?))?
}
