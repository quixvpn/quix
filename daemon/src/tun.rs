use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::net::Ipv4Addr;
use tun_rs::{AsyncDevice, DeviceBuilder};

pub const INTERFACE_NAME: &str = "quix";
pub const PREFIX_LEN: u8 = 10;

/// Derives a stable virtual IPv4 address from a peer's public key,
/// inside the CGNAT range 100.64.0.0/10.
pub fn virtual_ipv4(public_key: &[u8]) -> Ipv4Addr {
	let hash = Sha256::digest(public_key);

	const BASE: u32 = (100 << 24) | (64 << 16); // 100.64.0.0
	const HOST_MASK: u32 = (1 << 22) - 1; // 22 bits of host space

	let hash_u32 = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]);
	Ipv4Addr::from(BASE | (hash_u32 & HOST_MASK))
}

/// Creates the "quix" TUN interface with the given virtual IP already assigned and up.
pub fn create(virtual_ip: Ipv4Addr) -> Result<AsyncDevice> {
	DeviceBuilder::new()
		.name(INTERFACE_NAME)
		.ipv4(virtual_ip.to_string(), PREFIX_LEN, None)
		.mtu(1400)
		.build_async()
		.context("creating TUN device (run as root?)")
}