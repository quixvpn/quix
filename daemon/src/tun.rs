use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::net::Ipv4Addr;
use tun_rs::{AsyncDevice, DeviceBuilder};

pub const DEFAULT_INTERFACE_NAME: &str = "quix";
pub const PREFIX_LEN: u8 = 10;

/// Overridable so two daemons can run on one host for testing, alongside the
/// QUIX_KEY_PATH / QUIX_NETWORK_PATH / QUIX_SOCKET overrides.
pub fn interface_name() -> String {
	std::env::var("QUIX_IFACE").unwrap_or_else(|_| DEFAULT_INTERFACE_NAME.to_string())
}

/// 1280 is forced on us from below: it is RFC 8200's minimum IPv6 link MTU,
/// Windows applies our MTU to IPv6 as well as IPv4, and it rejects anything
/// smaller with ERROR_INVALID_PARAMETER. It is also what Tailscale and
/// WireGuard use, so it is well-trodden.
///
/// It sits slightly above what a *freshly established* QUIC link can carry in
/// one datagram (1162 bytes, from quinn's guaranteed 1200-byte packet), so
/// full-size packets can be dropped until path MTU discovery raises the limit
/// — a second or two, which TCP retransmits through. `serve_link` warns when a
/// link settles below 1280, and the `too big` counter in `quix status` shows
/// whether it is still happening.
pub const MTU: u16 = 1280;

/// Derives a stable virtual IPv4 address from a peer's public key,
/// inside the CGNAT range 100.64.0.0/10.
pub fn virtual_ipv4(public_key: &[u8]) -> Ipv4Addr {
	let hash = Sha256::digest(public_key);

	const BASE: u32 = (100 << 24) | (64 << 16); // 100.64.0.0
	const HOST_MASK: u32 = (1 << 22) - 1; // 22 bits of host space

	let hash_u32 = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]);
	Ipv4Addr::from(BASE | (hash_u32 & HOST_MASK))
}

/// Reads the destination address out of an IPv4 packet, which is how we pick
/// the peer to forward it to. Returns None for anything that isn't IPv4.
pub fn dst_ipv4(packet: &[u8]) -> Option<Ipv4Addr> {
	if packet.len() < 20 || packet[0] >> 4 != 4 {
		return None;
	}
	Some(Ipv4Addr::new(
		packet[16], packet[17], packet[18], packet[19],
	))
}

/// Creates the "quix" TUN interface with the given virtual IP already assigned and up.
pub fn create(virtual_ip: Ipv4Addr) -> Result<AsyncDevice> {
	DeviceBuilder::new()
		.name(interface_name())
		.ipv4(virtual_ip.to_string(), PREFIX_LEN, None)
		.mtu(MTU)
		.build_async()
		.context("creating TUN device (run as root?)")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn virtual_ip_is_stable_and_inside_the_cgnat_range() {
		let key = [7u8; 32];
		let ip = virtual_ipv4(&key);

		assert_eq!(ip, virtual_ipv4(&key), "same key must give the same address");
		assert_ne!(ip, virtual_ipv4(&[8u8; 32]));

		// 100.64.0.0/10
		let octets = ip.octets();
		assert_eq!(octets[0], 100);
		assert!((64..128).contains(&octets[1]), "got {ip}");
	}

	#[test]
	fn dst_ipv4_reads_the_destination_field() {
		let mut packet = [0u8; 20];
		packet[0] = 0x45; // IPv4, 5-word header
		packet[16..20].copy_from_slice(&[100, 64, 1, 2]);

		assert_eq!(dst_ipv4(&packet), Some(Ipv4Addr::new(100, 64, 1, 2)));
	}

	#[test]
	fn dst_ipv4_rejects_short_and_non_ipv4_packets() {
		assert_eq!(dst_ipv4(&[0x45; 19]), None, "truncated header");

		let mut v6 = [0u8; 40];
		v6[0] = 0x60;
		assert_eq!(dst_ipv4(&v6), None, "IPv6 has no IPv4 destination");
	}
}
