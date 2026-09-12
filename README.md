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

Installing a service and editing the machine PATH both need Administrator, so
the script re-runs itself elevated and you get a UAC prompt. Its output is
transcribed back to the terminal you started from. Run it already elevated and
it just proceeds.

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
| `quix invite --expires 30 min` | …valid for a window you choose, instead of the default 5 minutes |
| `quix join <code>` | Join a network with an invite code |
| `quix leave` | Leave the current network and drop every link |
| `quix hostname <name>` | Set this machine's name on the mesh |
| `quix status` | This node, its addresses, and every peer's link state |
| `quix status -v` | Adds per-hop packet counters and full endpoint ids |
| `quix ping <peer-id>` | Probe a peer's link and report RTT |
| `quix set-operator <user>` | Let a local user run commands without sudo (Unix only) |
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
expires:     in 5 minutes (2026-09-12T18:35:02+00:00)
single use — redeeming it consumes it, whatever time is left

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

### Invites

An invite is a bearer token: whoever holds the code can join, so it is scoped as
tightly as it can be while still being useful.

```bash
quix invite                     # valid for 5 minutes
quix invite --expires 30 min    # min, hours or days
quix invite --expires 2 days
```

Three limits, all enforced by the coordinator that minted the code:

- **Single use.** Redeeming it consumes it, whatever time is left on the window.
  A code presented once is spent even if it was refused, so a rejected code
  cannot be kept and retried later.
- **Time limited.** Five minutes by default, 30 days at most. Past the window it
  fails exactly as an already-used code does — a joiner is never told which of
  the two it was.
- **Bound to one network.** A code is only good for the network it was minted
  under. Creating another network, joining someone else's, or leaving all
  invalidate outstanding invites, and the network is identified by a random id
  rather than its name, so creating `homelab` twice does not make the first
  network's codes work on the second.

Codes minted by versions before this was enforced carry no window and no
network, so they are dropped when `network.json` is read.

### Names

Every peer has a name under `.quix`. Set one when joining, or at any time after:

```bash
quix create homelab --hostname nas
quix join <code> --hostname laptop
sudo quix hostname nas          # set or change it later
```

A peer with no hostname still has one: the first 8 characters of its endpoint
id, which is derived from its key and so can never be spoofed or reassigned.
Both forms resolve.

The coordinator resolves collisions by suffix — two peers asking for `web`
become `web` and `web-1` — rather than refusing the join.

**A name belongs to the key that first claimed it.** Only that peer can change
it, and no roster push can move an existing name to a different key, including
one from the coordinator. That holds even after the owner leaves: the name stays
reserved, because a departure is only ever reported by the coordinator, and
freeing it would let a compromised one evict a peer and take its name. A machine
rebuilt under a new key reclaims its old name with `quix hostname <name>
--force`, which every other member reports rather than applying silently.

This is trust-on-first-use, with TOFU's limits: it protects bindings you have
already seen, not the first one, and two peers that joined at different times
can disagree without a signed roster. It is a cheaper mitigation than signing,
not a replacement for it.

### Resolving names

`.quix` names work with ordinary programs — `ping`, `ssh`, a browser — because
the daemon registers the zone with the system resolver at startup:

```bash
ping nas.homelab.quix
ssh user@nas.homelab.quix
```

On **Linux** that is a systemd-resolved routing domain (`~quix`) attached to the
`quix` interface, so only `.quix` comes to us and every other name resolves
exactly as before. Because the configuration is per-link, it disappears together
with the interface — a killed daemon leaves nothing stale behind. resolved is
told the port as well as the address, so the daemon answers on an unprivileged
one and needs no capability to bind sockets.

On **Windows** it is an NRPT rule scoped to `.quix`. Those live in the registry
and outlast the process, so the daemon removes any rule of its own at startup
before adding a fresh one, and removes it again on shutdown.

Neither needs privileges beyond what the TUN device already requires. If
registration fails — no systemd-resolved, for instance — the daemon logs a
warning and carries on; only OS-wide resolution is lost.

Before handing an address to the system resolver the daemon queries it, and
registers only an address that answers. Binding one proves nothing on its own:
it can belong to an interface whose IPv6 is switched off a moment later, or sit
behind a filter that drops the port. Either way the socket looks healthy from
the inside while every lookup times out, so the daemon used to report success
and leave names quietly broken. IPv6 is still preferred; it just has to work.

#### Another VPN's leak protection can break this

On Windows especially, VPN clients ship leak-protection settings that reach
across the whole machine, not just their own adapter:

- **IPv6 leak protection** unbinds IPv6 (`ms_tcpip6`) from *every* adapter,
  including `quix`. Our IPv6 mesh address then has no interface, no address and
  no route, which is exactly the case above.
- **DNS leak protection** filters UDP port 53 to everything but that VPN's own
  resolver. NRPT rules carry no port field, so the Windows listener has no
  choice but port 53, and there is nothing quix can do about this from inside.

Proton VPN's IPv6 leak protection does the first of these and is a confirmed
cause. To check:

```powershell
Get-NetAdapterBinding -ComponentID ms_tcpip6 -AllBindings | Format-Table Name, Enabled
```

If `quix` shows `False`, turn off the other VPN's IPv6 leak protection (or add
an exception for the `quix` adapter) and restart the daemon. The daemon now says
so in its log rather than claiming it registered successfully — see
`C:\ProgramData\quix\quixd.log`.

The resolver also stays reachable directly, which is the way to test it without
involving the system resolver at all:

```bash
dig @127.0.0.1 -p 5354 nas.homelab.quix AAAA   # host.network.quix
dig @127.0.0.1 -p 5354 nas.quix AAAA           # flat form, same answer
dig @127.0.0.1 -p 5354 dd0f06dd.quix A         # the fallback name works too
```

```powershell
nslookup -port=5354 -type=AAAA nas.homelab.quix 127.0.0.1
```

Names resolve as `host.network.quix`, and the flat `host.quix` works too, so a
name stays typable without remembering which network a peer is on. Because the
network name becomes a DNS label, `quix create` now requires one — letters,
digits and hyphens. Networks created before that rule are sanitised rather than
refused.

Override the testing address with `QUIX_DNS_ADDR`. The daemon additionally
listens on its mesh addresses, which is what the OS is pointed at: port 5354 on
Linux, and port 53 on Windows because NRPT rules carry no port field.

To see what Linux thinks:

```bash
resolvectl domain quix      # should list ~quix
resolvectl query nas.homelab.quix
```

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
connection itself, rather than by the socket's file permissions:

- **Read-only** commands (`status`, `ping`) are open to any local user.
- **Mutating** commands (`create`, `invite`, `join`, `leave`, `hostname`,
  `set-operator`) need root, or the configured operator, or — on Windows — an
  elevated caller.

The identity is the kernel's answer about the live connection, not anything the
client sends, so it cannot be forged. On Unix that is `SO_PEERCRED`. On Windows
the daemon impersonates the pipe client and reads the token behind it: the pipe
can also name the client's PID, but looking a token up by PID is racy — the
process can exit and its PID be reused before the check — so the connection is
asked instead. A client that connects anonymously has no token to read, and
counts as unidentified, which is refused.

The user who runs the installer becomes the operator automatically, on both
platforms. On Unix that is `$SUDO_USER`'s uid; on Windows it is the installing
account's SID, and the installer refuses anything that is not a real user
account — a group or a well-known SID like `Everyone` is far too broad to hand
this to.

Authorize someone else with:

```bash
sudo quix set-operator alice
```

`set-operator` is Unix-only for now: it takes a username, and the Windows
operator is a SID. Reinstalling is how you change it there. A SID rather than a
uid because Windows has nothing like a uid — and it is the sounder identity
anyway, since a deleted-and-recreated account gets a fresh SID, where a uid can
be recycled and silently hand authority to a different person.

This is the model Tailscale uses. A Unix group (the `docker` approach) can't
distinguish reading status from changing what the machine belongs to, needs a
fresh login session to take effect, and has no Windows equivalent.

### Elevation on Windows

There is no `sudo`: a process cannot gain privileges, only start one that has
them. So rather than failing with "run this from an elevated terminal", the CLI
re-runs itself through `ShellExecuteExW` with the `runas` verb — the same
mechanism as the shell's own *Run as administrator* — and you get a UAC prompt.

Only commands that genuinely need it ask. Everything else runs with no prompt,
because a dialog in front of `quix status` teaches people to click through them:

| Prompts | Why |
|---|---|
| `service start` \| `stop` \| `restart` \| `enable` \| `disable` | Service Control Manager writes |
| `update` | Stops the service and replaces binaries in `Program Files` |

`status`, `ping`, `version`, `service status` and `update --check` never prompt.

The membership commands — `create`, `invite`, `join`, `leave`, `hostname` — are
the daemon's decision rather than the OS's, so they are **tried first and only
prompt if the daemon actually refuses**. Since the installing account is already
the operator, day to day they never prompt at all; another account on the same
machine gets a prompt exactly when it would change the outcome. Asking up front
would put a dialog in front of the one person who does not need one.

Nothing has happened when that refusal arrives — authorization is checked before
the daemon acts — so the elevated retry repeats no work.

The elevated copy cannot write to your console — Windows does not let a
higher-integrity process attach to a lower-integrity one — so it runs hidden
with its output pointed at a file, which the original process prints when it
exits, along with its exit code. From the terminal the command simply runs,
after a prompt. Declining the prompt reports that and exits non-zero.

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

- **Peers are not checked against the addresses they send from.** An inbound
  packet is written to the TUN whatever source address it claims, so any member
  can pose as any other at the IP layer. WireGuard's `AllowedIPs` is the check
  this is missing; the routing table needed to do it already exists.
- **The control socket is open to every local user.** Both platforms — `0666` on
  the Unix socket, a null DACL on the Windows pipe — and authorization is what
  actually guards it, not the permissions. That is deliberate, so that reading
  state needs no privilege, but it does mean a bug in `authz` is the only thing
  between a local process and a mutating command. Tightening both to a dedicated
  group and to `Users`-read / `Administrators`-write is still to do.
- **The Windows operator can only be set by installing.** The installer records
  the installing account's SID, which covers the common case, but `set-operator`
  takes a username and has no SID equivalent yet — so changing it afterwards, or
  granting a second account, means reinstalling.
- **One network per node.** `Membership` holds a single network; there's no
  `quix leave <name>` because there's nothing to disambiguate.
- **The roster is unsigned.** Members trust the coordinator by identity alone,
  and it must be online to admit anyone. No DHT-published signed record yet, so
  admission doesn't survive the coordinator being away.
- **macOS has no resolver integration.** Linux and Windows register the zone;
  macOS would need its own mechanism.
- **No `quix up` / `down`,** and no way to pause without stopping the daemon.
- **Path MTU.** A link that settles below 1280 bytes drops large packets rather
  than fragmenting them.

Planned work and the reasoning behind it lives in [ROADMAP.md](ROADMAP.md).

## Contributing

`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test --workspace` both need to pass; CI runs them on every push. The
codebase indents with tabs (`rustfmt.toml` sets `hard_tabs`), though it is not
uniformly `cargo fmt`-clean yet.
