# Roadmap

Open work, roughly in the order it should happen.

---

## Notes for remembering

### 1. Settle the datagram limit

**The one functional unknown.** `quix status` reports a `datagram NNNN` line per
peer. Read it:

- **≥ 1280** — nothing to do. The `too big` drops seen during testing (21 and 36
  packets) were the first second of each link, before path MTU discovery raised
  the limit, and TCP retransmits through that.
- **stuck at 1162** — every full-size packet is dropped. Ping works, interactive
  SSH works, and a file transfer hangs forever, because we drop silently instead
  of sending ICMP "fragmentation needed", so TCP black-holes rather than backing
  off. That is not shippable.

The fix, if it's stuck: fragment oversized packets across datagrams and
reassemble on the far side. Bounded work — the TUN MTU is 1280 and the floor is
1162, so two fragments always suffice.

Background: the MTU can't go below 1280 (RFC 8200's IPv6 minimum, which Windows
enforces on the interface), and QUIC only guarantees a 1200-byte packet, which
measures out to 1162 bytes of datagram payload. The two bounds genuinely
overlap; PMTU discovery is what normally resolves it.

### 2. One state-directory variable instead of one per file

`v0.1.0` shipped broken because the systemd unit named `QUIX_KEY_PATH` and
`QUIX_NETWORK_PATH` but not `QUIX_SETTINGS_PATH`, added later. The daemon fell
back to `dirs::config_dir()` — `/root/.config` — which `ProtectHome=yes` makes
unreadable, so it exited with a bare `Permission denied` on every start.

Three variables means every new state file is a chance to forget one, in two
places (the unit and the Windows service's registry `Environment` key). A single
`QUIX_STATE_DIR` that the individual paths derive from removes the whole class,
with the per-file overrides kept for tests.

### 3. `quix update` cannot fix service configuration

It swaps binaries only. Service config — the systemd unit, the Windows
registry environment — is written by the installer, so any release that changes
one needs `install.sh` / `install.ps1` re-run rather than `quix update`. This
has already bitten twice: a unit missing `QUIX_SETTINGS_PATH`, and again adding
`QUIX_LOG_PATH`.

`update` should at minimum compare the installed service config against the one
in the release and say "re-run the installer" when they differ, rather than
reporting success and leaving the daemon misconfigured.

### 4. A clearer error when the TUN name is taken

`Device or resource busy (os error 16)` is what you get when another quixd
already holds the interface — the normal case for anyone who ran from a build
tree before installing. It should name the cause and the fix. `tun-rs` also has
`reuse_dev`, so adopting an orphaned interface with no process behind it is
possible, though taking one over while another daemon holds it would be wrong.

---

## Soon after

### Windows service logging

Everything the daemon prints — `peer linked`, `route added`, the datagram
warning, route failures — goes nowhere once it runs under the Service Control
Manager, because a service has no console. When a Windows user reports "it
doesn't work" there is nothing to ask them for. Linux is fine: journald captures
stdout, so `journalctl -u quixd -f` shows everything.

Fix: redirect stdout/stderr to a file under `C:\ProgramData\quix\` at service
startup (`SetStdHandle`), or add a small logging shim that writes to both.

### Windows caller authorization

`authz.rs` enforces nothing on Windows. The named pipe gives us the client's PID,
but turning that into a user SID needs `OpenProcessToken` +
`GetTokenInformation`. Until then any local user on a Windows host can create,
join or leave networks. Unix is fully enforced via `SO_PEERCRED`.

### Report the daemon's version too

`quix version` reports the CLI only. `quix update` swaps both binaries, so a
partial failure can leave `quix` and `quixd` on different versions with nothing
saying so. Adding the daemon's version to the `Status` response would surface
exactly the failure mode the update path can produce.

### Exercise `quix update` for real

The API call and its error handling are verified, but the download, checksum
verification, service stop and binary swap have never run against a real asset —
there were no releases when it was written. Tag `v0.1.0` then `v0.1.1` fairly
promptly just to prove the path, rather than finding out from a user.

---

## Bigger pieces

### Multiple networks per node

`Membership` holds exactly one network, which is why `quix leave` takes no
argument. Supporting several means keying it by network id and threading that
through routing, status and every command. Most of the remaining UX ideas assume
this is done first.

### Signed roster, published to a DHT

Members trust the coordinator by identity alone, and it must be online to admit
anyone. Publishing a signed network record (pkarr, as iroh already uses for
address discovery) would let admission survive the coordinator being away, and
make the roster verifiable rather than merely received over a trusted link.

### The resolver answers UDP only

If systemd-resolved ever retries a `.quix` query over TCP — on truncation, or
while downgrading through its feature levels — nothing is listening. Answers are
a single record today so the truncation bit should never be set, but it is a
gap, and a host configured with `DNSSEC=yes` would also fail to validate a zone
with no chain of trust.

### Resolver integration on macOS

Linux registers a systemd-resolved routing domain and Windows an NRPT rule.
macOS needs its own mechanism — a resolver file under `/etc/resolver/quix`, or
the equivalent via `scutil`.

### Hosts without systemd-resolved

Linux registration assumes systemd-resolved. On a host using plain
`/etc/resolv.conf` or `resolvconf`, the daemon warns and `.quix` only resolves
through its own port. Rayfish shares `resolv.conf` with other VPNs rather than
fighting over it, which is the model to copy if this matters.

### Converging a refused name rebinding

A member that has pinned `nas` to one key refuses a push binding it to another,
which is the point — but it then disagrees with the coordinator permanently.
`quix hostname --force` fixes the coordinator's view; there is no way to tell an
individual member to accept a rebinding it has refused. Signed rosters would
settle this properly; a local pin-reset command would be the cheap stopgap.

### Hostnames assume one network

Names live in the single flat `Membership`, and the zone is `name.quix`. With
several networks a peer could hold a different name in each, and the zone wants
to be `name.network.quix`. The resolver already matches the zone as a suffix so
the extra label can be added without reworking the query path.

### Standby and member removal

- `quix up` / `quix down` to drop the data plane while keeping peer connections
  warm, so coming back is instant and needs no root.
- `quix kick <member>` for the coordinator, removing someone from the signed
  roster so every other member disconnects them.

### macOS

`tun-rs` supports it, and nothing in the design is Linux- or Windows-specific
beyond `routes.rs` and the service integration. Needs a launchd plist, the
`route(8)` calls in place of `ip`/`netsh`, and a machine to test on.

---

## Accepted limitations

Not bugs, and not currently planned to change:

- **Coordinator must be online to admit new members.** Existing members reconnect
  without it; only joins need it. Fixed by the signed-roster item above.
- **IPv4 addresses can collide.** 24 bits of host space in `10.0.0.0/8` means a
  derived address could clash with something on a user's LAN. IPv6 is the primary
  family precisely because 120 bits makes this impossible there, and a peer stays
  reachable over v6 when its v4 address is contested.
- **Host firewalls must permit the mesh interface.** The installers don't touch
  firewall rules. On Fedora that means adding `quix` to a permissive zone; on
  Windows, an inbound allow rule for the interface. Tailscale automates this; we
  don't yet.
