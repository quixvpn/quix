use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use tun_rs::{AsyncDevice, DeviceBuilder};

pub const DEFAULT_INTERFACE_NAME: &str = "quix";

/// Overridable so two daemons can run on one host for testing, alongside the
/// QUIX_KEY_PATH / QUIX_NETWORK_PATH / QUIX_SOCKET overrides.
pub fn interface_name() -> String {
	std::env::var("QUIX_IFACE").unwrap_or_else(|_| DEFAULT_INTERFACE_NAME.to_string())
}

/// 1280 is RFC 8200's minimum IPv6 link MTU. Windows applies our MTU to IPv6 as
/// well as IPv4 and rejects anything smaller, and it is what Tailscale and
/// WireGuard use. A freshly established QUIC link carries only 1162 bytes per
/// datagram, so full-size packets can drop until path MTU discovery raises the
/// limit a second or two later; `serve_link` warns if a link settles below it.
pub const MTU: u16 = 1280;

/// Both addresses are host routes. We install one route per member instead of
/// claiming a whole prefix — claiming `100.64.0.0/10` is what made us swallow
/// traffic that was never ours.
pub const V4_PREFIX_LEN: u8 = 32;
pub const V6_PREFIX_LEN: u8 = 128;

/// Derives the overlay IPv6 address from a peer's public key, inside `200::/7`.
///
/// This is the primary address. The range is unallocated, so nothing else uses
/// it, and 120 key-derived bits make collisions impossible in practice — unlike
/// any IPv4 range, all of which are contested.
pub fn virtual_ipv6(public_key: &[u8]) -> Ipv6Addr {
	let hash = Sha256::digest(public_key);

	let mut octets = [0u8; 16];
	octets.copy_from_slice(&hash[..16]);
	// Top 7 bits select 200::/7; the remaining 121 are derived from the key.
	octets[0] = 0x02;

	Ipv6Addr::from(octets)
}

/// Derives the compatibility IPv4 address from a peer's public key, inside
/// `10.0.0.0/8`, for applications that cannot speak IPv6.
///
/// Deliberately NOT in `100.64.0.0/10`: Tailscale drops any packet sourced from
/// that range which did not arrive on its own interface, which silently kills
/// every mesh packet on a host running it.
pub fn virtual_ipv4(public_key: &[u8]) -> Ipv4Addr {
	let hash = Sha256::digest(public_key);

	const BASE: u32 = 10 << 24; // 10.0.0.0
	const HOST_MASK: u32 = (1 << 24) - 1; // 24 bits of host space

	let host = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]) & HOST_MASK;
	// Keep the last octet out of .0 and .255: plenty of software still treats
	// those as network and broadcast addresses regardless of the prefix length.
	let host = (host & !0xff) | (host & 0xff).clamp(1, 254);

	Ipv4Addr::from(BASE | host)
}

/// Both overlay addresses for a peer.
pub fn virtual_addrs(public_key: &[u8]) -> (Ipv4Addr, Ipv6Addr) {
	(virtual_ipv4(public_key), virtual_ipv6(public_key))
}

/// Reads the destination address out of an IP packet, which is how we pick the
/// peer to forward it to. Handles both families; returns None for anything else.
pub fn dst_addr(packet: &[u8]) -> Option<IpAddr> {
	match packet.first()? >> 4 {
		4 if packet.len() >= 20 => Some(IpAddr::V4(Ipv4Addr::new(
			packet[16], packet[17], packet[18], packet[19],
		))),
		6 if packet.len() >= 40 => {
			let mut octets = [0u8; 16];
			octets.copy_from_slice(&packet[24..40]);
			Some(IpAddr::V6(Ipv6Addr::from(octets)))
		}
		_ => None,
	}
}

/// Creates the TUN interface carrying both overlay addresses as host routes.
pub fn create(v4: Ipv4Addr, v6: Ipv6Addr) -> Result<AsyncDevice> {
	DeviceBuilder::new()
		.name(interface_name())
		.ipv4(v4.to_string(), V4_PREFIX_LEN, None)
		.ipv6(v6.to_string(), V6_PREFIX_LEN)
		.mtu(MTU)
		.build_async()
		.context("creating TUN device (run as root?)")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ipv6_is_stable_and_inside_200_over_7() {
		let key = [7u8; 32];
		let ip = virtual_ipv6(&key);

		assert_eq!(ip, virtual_ipv6(&key), "same key must give the same address");
		assert_ne!(ip, virtual_ipv6(&[8u8; 32]));

		// 200::/7 means the top 7 bits are 0000001.
		assert_eq!(ip.octets()[0] >> 1, 0b0000001, "got {ip}");
	}

	#[test]
	fn ipv4_is_stable_and_inside_10_over_8() {
		let key = [7u8; 32];
		let ip = virtual_ipv4(&key);

		assert_eq!(ip, virtual_ipv4(&key));
		assert_ne!(ip, virtual_ipv4(&[8u8; 32]));
		assert_eq!(ip.octets()[0], 10, "got {ip}");
	}

	#[test]
	fn ipv4_never_ends_in_zero_or_broadcast() {
		// Sweep enough keys to hit both clamped ends.
		for n in 0..=u8::MAX {
			let last = virtual_ipv4(&[n; 32]).octets()[3];
			assert!(last != 0 && last != 255, "key {n} gave a .{last} address");
		}
	}

	#[test]
	fn ipv4_stays_off_tailscales_range() {
		// 100.64.0.0/10 is dropped by Tailscale's anti-spoof rule on any host
		// running it, which silently kills the whole mesh.
		for n in 0..=u8::MAX {
			let octets = virtual_ipv4(&[n; 32]).octets();
			assert_ne!(octets[0], 100, "landed in CGNAT space");
		}
	}

	#[test]
	fn dst_addr_reads_ipv4_destinations() {
		let mut packet = [0u8; 20];
		packet[0] = 0x45; // IPv4, 5-word header
		packet[16..20].copy_from_slice(&[10, 1, 2, 3]);

		assert_eq!(
			dst_addr(&packet),
			Some(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)))
		);
	}

	#[test]
	fn dst_addr_reads_ipv6_destinations() {
		let mut packet = [0u8; 40];
		packet[0] = 0x60; // IPv6
		let want: Ipv6Addr = "200:1122:3344:5566:7788:99aa:bbcc:ddee".parse().unwrap();
		packet[24..40].copy_from_slice(&want.octets());

		assert_eq!(dst_addr(&packet), Some(IpAddr::V6(want)));
	}

	#[test]
	fn dst_addr_rejects_truncated_and_unknown_packets() {
		assert_eq!(dst_addr(&[]), None);
		assert_eq!(dst_addr(&[0x45; 19]), None, "truncated IPv4 header");
		assert_eq!(dst_addr(&[0x60; 39]), None, "truncated IPv6 header");
		assert_eq!(dst_addr(&[0x05; 40]), None, "not a known IP version");
	}
}
