# Quix

**A peer-to-peer mesh VPN.** Your machines get a private network of their own —
laptop, desktop, server, a friend's box — talking to each other as if they were
on the same switch, wherever they actually are.

There is no server to host and no account to create. One person runs a command,
shares a code, and the network exists.

```bash
quix create homelab       # you now have a private network
quix invite               # mint a one-time code to hand out
quix join <code>          # a friend joins with it
ping 200:7c10:5e8b:…      # you reach each other
```

Built on [iroh](https://www.iroh.computer/) for identity, NAT traversal and
encrypted transport. [Blindspot](https://github.com/neozmmv/blindspot) was the
first take on this idea, over self-hosted rendezvous and Tor.

> [!WARNING]
> **Experimental, pre-1.0, and not audited.** The wire format and on-disk
> layout still change without migration paths. Good for a homelab or a few
> friends; not for anything that matters yet.

---

## How it works

Every machine runs a small daemon (`quixd`) that creates a TUN device, captures
IP packets, and carries them over iroh's QUIC connections. Everything else is
`quix`, an unprivileged CLI that talks to the daemon over a local socket.

1. **Identity.** Each node has a persistent keypair. Its overlay addresses are
   derived from the public key — stable, collision-free, and assigned with no
   coordinator handing anything out.
2. **Create.** One peer starts a network and becomes its coordinator. It holds
   the roster and mints invites.
3. **Join.** A new peer redeems a one-time invite code. The coordinator verifies
   it, burns it, adds them to the roster, and pushes the updated roster to
   everyone else.
4. **Mesh.** Every member connects directly to every other member. The
   coordinator is not in the data path — it only gates admission.
5. **Route.** One reader on the TUN device looks up each packet's destination
   and forwards it to the peer that owns that address, as a QUIC datagram.

### Addressing

Each peer gets two addresses, both derived from its public key:

| | Range | Purpose |
|---|---|---|
| **IPv6** | `200::/7` | Primary. 120 key-derived bits, so collisions don't happen. |
| **IPv4** | `10.0.0.0/8` | Compatibility, for software that can't speak IPv6. |

Only **host routes** are installed — a `/32` and a `/128` per member, never the
whole prefix. Claiming a wide prefix means swallowing traffic that isn't yours,
which breaks other networks on the same machine.

`100.64.0.0/10` is deliberately avoided. Tailscale drops any packet sourced from
that range which didn't arrive on its own interface, so a mesh living there is
silently dead on every host running Tailscale. Quix and Tailscale coexist fine.

### Transport

Packets travel as QUIC **datagrams**, not stream data — one packet, one
datagram. A reliable stream would add head-of-line blocking on top of whatever
the tunnelled protocol already does.

The TUN MTU is 1280 (the IPv6 minimum; Windows won't accept less). A freshly
established QUIC link only carries 1162 bytes per datagram until path MTU
discovery raises it, so large packets can drop for the first second of a link's
life. `quix status` flags any link that stays below the MTU.

---

## Install

### Linux

```bash
curl -fsSL https://raw.githubusercontent.com/quixvpn/quix/master/scripts/install.sh | sudo bash
```

Or from a clone, which also lets you install a local build:

```bash
sudo ./scripts/install.sh            # from the latest release
sudo ./scripts/install.sh --local    # from a local build
sudo ./scripts/install.sh --uninstall
```

Installs both binaries to `/usr/local/bin`, registers `quixd` as a systemd
service, starts it, and makes whoever ran `sudo` the operator so day-to-day
commands don't need root.

Prebuilt for **x86_64** and **aarch64**. Override with `INSTALL_DIR` or `REPO`.

The service runs with `CapabilityBoundingSet=CAP_NET_ADMIN` — enough for the TUN
device and its routes, and nothing else — with state in `/var/lib/quix` and its
socket at `/run/quix/quixd.sock`.

### Windows

From an **elevated** PowerShell:

```powershell
irm https://raw.githubusercontent.com/quixvpn/quix/master/scripts/install.ps1 | iex
```

Or from a clone, which is the only way to pass flags:

```powershell
.\scripts\install.ps1              # from the latest release
.\scripts\install.ps1 -Local       # from a local build
.\scripts\install.ps1 -Uninstall
```

Installs to `Program Files\quix`, adds it to the machine PATH, and registers
`quixd` as a LocalSystem service with automatic restart. State lives in
`C:\ProgramData\quix`. Open a new terminal afterwards to pick up the PATH.

Windows is **experimental** — see [Known gaps](#known-gaps).

### From source

Needs a Rust toolchain (2021 edition).

```bash
cargo build --release       # or: cargo build
cargo test --workspace
```

To run the daemon straight from a build tree, without installing:

```bash
sudo env "PATH=$PATH" QUIX_SOCKET=/tmp/quix.sock ./target/debug/quixd
QUIX_SOCKET=/tmp/quix.sock ./target/debug/quix status
```

---

## Commands

| Command | What it does |
|---|---|
| `quix create <name>` | Start a network and become its coordinator |
| `quix invite` | Mint a one-time invite code (coordinator only) |
| `quix join <code>` | Join a network with an invite code |
| `quix leave` | Leave the current network and drop every link |
| `quix status` | This node, its addresses, and every peer's link state |
| `quix status -v` | Adds per-hop packet counters and full endpoint ids |
| `quix ping <peer-id>` | Probe a peer's link and report RTT |
| `quix set-operator <user>` | Let a local user run commands without sudo |
| `quix service status` | Whether the daemon runs now, and whether it starts at boot |
| `quix service start` \| `stop` \| `restart` | Control the daemon now |
| `quix service enable` \| `disable` | Control whether it starts at boot |
| `quix update` | Install the latest release over this one |
| `quix version` | Installed version, matching the release tag |

A typical session:

```console
$ quix create homelab
network created, you are the coordinator

$ quix invite
invite code: 7hmYoctBr87SmrNCLPtk6op4V36pf18JCkEDWsva6K3PVPHtQf2UWb51hcQrL17WRw

$ quix status
network  homelab  (coordinator)
address  22b:a3a2:ef78:522c:2f10:b6c1:73f8:289c
         10.43.163.162

peers  1/1 linked
  ● 27d:38ff:65bd:edcd:1286:dd57:2ee9:db13
    10.125.56.254    dd0f06…48d2
```

`●` means the data-plane link is up, `○` means the peer is known but not
currently reachable.

### Controlling the daemon

```bash
quix service status         # running: yes / at boot: enabled
sudo quix service stop      # stop it now; it still comes back at boot
sudo quix service disable   # stop it coming back at boot; leaves it running now
sudo quix service start     # start it, and wait until it actually answers
```

"Running now" and "starts at boot" are separate, so `stop` alone doesn't survive
a reboot and `disable` alone doesn't stop anything today — use both to turn it
off for good.

`start` and `restart` return only once the daemon is answering, not merely once
the process exists. The service manager reports success immediately, but the
daemon still has to pick a relay, bring the TUN up and bind its socket, so a
bare `systemctl start` followed by `quix status` can race.

### Updating

```bash
quix version            # quix v0.1.0
quix update --check     # what's available, installs nothing
sudo quix update        # stop the service, swap both binaries, start it again
sudo quix update --force   # reinstall the same version
```

It downloads both binaries for your platform, verifies their SHA-256 against
the release, and only then stops the service and swaps them — a failed download
can't leave you half-updated. The old binaries are renamed aside rather than
overwritten, since a running executable can't be replaced in place on Windows.

Updating needs write access to wherever quix is installed, so `sudo` on Linux
and an elevated PowerShell on Windows.

### Diagnosing

`quix status -v` reports counters at each hop a packet takes, in order, so the
first zero is the hop that's failing:

```
outbound  tun read 155  ->  sent 30
inbound   received 29  ->  tun write 29
dropped   not a member 104  too big 21
```

- `not a member` — destination isn't in the roster. IPv6 neighbour discovery and
  multicast land here routinely; that's expected.
- `no link` — the peer is a member but isn't connected yet.
- `too big` — the packet exceeded the link's datagram limit and was dropped.
- `send failed` / `tun write failed` — the transport or the kernel refused it.

---

## Permissions

The daemon authorizes each command by the **caller's identity**, read from the
socket itself, rather than by the socket's file permissions:

- **Read-only** commands (`status`, `ping`) are open to any local user.
- **Mutating** commands (`create`, `invite`, `join`, `leave`, `set-operator`)
  need root or the configured operator.

The user who runs the installer becomes the operator automatically. Authorize
someone else with:

```bash
sudo quix set-operator alice
```

This is the model Tailscale uses. A Unix group (the `docker` approach) can't
distinguish reading status from changing what the machine belongs to, needs a
fresh login session to take effect, and has no Windows equivalent.

---

## Configuration

Environment variables, mostly for running several daemons on one host:

| Variable | Default |
|---|---|
| `QUIX_SOCKET` | `/run/quix/quixd.sock`, or a named pipe on Windows |
| `QUIX_KEY_PATH` | `<config dir>/quix/key` |
| `QUIX_NETWORK_PATH` | `<config dir>/quix/network.json` |
| `QUIX_SETTINGS_PATH` | `<config dir>/quix/settings.json` |
| `QUIX_IFACE` | `quix` |

`scripts/two-node-test.sh` uses these to run two daemons side by side on one
machine — useful for exercising the control plane, though not packet forwarding,
since both addresses are local to that host.

---

## Known gaps

Being explicit about what isn't built yet:

- **Windows has no caller authorization.** The named pipe gives us the client's
  PID, but turning that into a user SID needs token FFI that isn't written.
  Until then any local user on Windows can run any command.
- **One network per node.** `Membership` holds a single network; there's no
  `quix leave <name>` because there's nothing to disambiguate.
- **The roster is unsigned.** Members trust the coordinator by identity alone,
  and it must be online to admit anyone. No DHT-published signed record yet, so
  admission doesn't survive the coordinator being away.
- **No name resolution.** Peers are reached by address, not by name.
- **No `quix up` / `down`,** and no way to pause without stopping the daemon.
- **Path MTU.** A link that settles below 1280 bytes drops large packets rather
  than fragmenting them.

Planned work and the reasoning behind it lives in [ROADMAP.md](ROADMAP.md).

## Contributing

`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test --workspace` both need to pass; CI runs them on every push. The
codebase indents with tabs (`rustfmt.toml` sets `hard_tabs`), though it is not
uniformly `cargo fmt`-clean yet.
