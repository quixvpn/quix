#!/usr/bin/env bash
# Brings up two daemons on this host and walks create → invite → join, then
# shows whether the mesh linked up. Needs root for the TUN devices.
#
#   sudo ./scripts/two-node-test.sh
#
# Each node gets its own key, roster, IPC socket and TUN interface via the
# QUIX_* overrides, so they behave like two separate machines. Packet
# forwarding is NOT exercised: both virtual addresses are local to this host,
# so the kernel short-circuits them instead of routing through the tunnel.
# What this does verify is the control plane and the data-plane links.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=/tmp/quix-two-node
QUIXD=target/debug/quixd
QUIX=target/debug/quix

[[ $EUID -eq 0 ]] || { echo "needs root (TUN creation): sudo $0" >&2; exit 1; }
[[ -x $QUIXD && -x $QUIX ]] || { echo "build first: cargo build" >&2; exit 1; }

rm -rf "$ROOT"; mkdir -p "$ROOT"/{a,b}

node_env() { # node_env <name>
	export QUIX_KEY_PATH="$ROOT/$1/key"
	export QUIX_NETWORK_PATH="$ROOT/$1/network.json"
	export QUIX_SOCKET="$ROOT/$1.sock"
	export QUIX_IFACE="quix$1"
}

cleanup() {
	echo; echo "--- shutting down ---"
	kill "${PID_A:-}" "${PID_B:-}" 2>/dev/null || true
	wait 2>/dev/null || true
}
trap cleanup EXIT

start() { # start <name> -> pid
	( node_env "$1"; exec "$QUIXD" ) >"$ROOT/$1.log" 2>&1 &
	echo $!
}

on() { # on <name> <args...>
	( node_env "$1"; shift; "$QUIX" "$@" )
}

wait_for_socket() { # wait_for_socket <name>
	# Generous: the daemon waits for a relay before it serves, so that its
	# address is published by the time anyone tries to resolve it.
	for _ in $(seq 1 150); do
		[[ -S "$ROOT/$1.sock" ]] && return 0
		sleep 0.2
	done
	echo "node $1 never came up; see $ROOT/$1.log" >&2
	tail -20 "$ROOT/$1.log" >&2
	exit 1
}

echo "--- starting both nodes ---"
PID_A=$(start a); PID_B=$(start b)
wait_for_socket a; wait_for_socket b

echo "--- A creates a network ---"
on a create gaming

echo "--- A mints an invite ---"
CODE=$(on a invite | awk '{print $NF}')
echo "invite: $CODE"

echo "--- B joins with it ---"
on b join "$CODE"

echo "--- waiting for the mesh to link ---"
for _ in $(seq 1 30); do
	on a status 2>/dev/null | grep -q '●' && break
	sleep 1
done

echo; echo "=== node A ==="; on a status
echo; echo "=== node B ==="; on b status

A_ID=$(on a status | awk '/^id/{print $2}')
echo; echo "--- B probes A ---"; on b ping "$A_ID" || true

echo; echo "logs: $ROOT/a.log $ROOT/b.log"
echo "a '●' next to a peer means the data-plane link is up."
