#!/usr/bin/env bash
# Two daemons on this host, then `quix file` between them for real: the daemons
# run as root, and every file command runs as the user who invoked sudo — the
# same split as an installed system, and the one the privilege rules are about.
#
#   cargo build && sudo ./scripts/two-node-file-test.sh
#
# Checks: a transfer that arrives intact and belongs to the user, a rejection,
# a sender withdrawing while waiting, a receiver dying mid-transfer, and a file
# the user cannot read. Set EXPIRY=1 to also wait out a one-minute offer.
#
# Each node gets its own key, roster, settings, socket and TUN interface via
# the QUIX_* overrides. Transfers use their own quix-file/0 connections, found
# by iroh's discovery like any other, so this needs internet access for the
# relay just as two-node-test.sh does.
set -uo pipefail

cd "$(dirname "$0")/.."
ROOT=/tmp/quix-file-two-node
QUIXD=$PWD/target/debug/quixd
QUIX=$PWD/target/debug/quix

[[ $EUID -eq 0 ]] || { echo "needs root (TUN creation): sudo $0" >&2; exit 1; }
[[ -n ${SUDO_USER:-} && $SUDO_USER != root ]] || {
	echo "run through sudo from your own account: the file commands run as you" >&2
	exit 1
}
[[ -x $QUIXD && -x $QUIX ]] || { echo "build first: cargo build" >&2; exit 1; }

rm -rf "$ROOT"; mkdir -p "$ROOT"/{a,b}
# On disk rather than /tmp, which is often RAM: the transfers here are large.
WORK=$(sudo -u "$SUDO_USER" mktemp -d -p /var/tmp quix-file-test.XXXXXX)
sudo -u "$SUDO_USER" mkdir -p "$WORK"/{a,b}

PASSED=0; FAILED=0
pass() { echo "  PASS  $*"; PASSED=$((PASSED + 1)); }
fail() { echo "  FAIL  $*"; FAILED=$((FAILED + 1)); }

node_env() { # node_env <name>
	export QUIX_KEY_PATH="$ROOT/$1/key"
	export QUIX_NETWORK_PATH="$ROOT/$1/network.json"
	export QUIX_SETTINGS_PATH="$ROOT/$1/settings.json"
	export QUIX_SOCKET="$ROOT/$1.sock"
	export QUIX_IFACE="quix$1"
}

cleanup() {
	echo; echo "--- shutting down ---"
	kill "${PID_A:-}" "${PID_B:-}" 2>/dev/null || true
	wait "${PID_A:-}" "${PID_B:-}" 2>/dev/null || true
	echo "logs: $ROOT/a.log $ROOT/b.log   files: $WORK"
}
trap cleanup EXIT

start() { # start <name> -> pid
	( node_env "$1"; exec "$QUIXD" ) >"$ROOT/$1.log" 2>&1 &
	echo $!
}

on() { # on <name> <args...>   as root, for setting the nodes up
	( node_env "$1"; shift; "$QUIX" "$@" )
}

as_user() { # as_user <name> <dir> <args...>   as the invoking user, from <dir>
	local node=$1 dir=$2; shift 2
	( cd "$dir" && sudo -u "$SUDO_USER" env QUIX_SOCKET="$ROOT/$node.sock" "$QUIX" "$@" )
}

wait_for_socket() { # wait_for_socket <name>
	for _ in $(seq 1 150); do
		[[ -S "$ROOT/$1.sock" ]] && return 0
		sleep 0.2
	done
	echo "node $1 never came up; see $ROOT/$1.log" >&2
	tail -20 "$ROOT/$1.log" >&2
	exit 1
}

# The id of the first offer waiting on node b, from the plain table.
first_offer_on_b() {
	as_user b "$WORK/b" file list | awk 'NR == 2 { print $1 }'
}

wait_for_offer_on_b() {
	local id=""
	for _ in $(seq 1 100); do
		id=$(first_offer_on_b)
		[[ -n $id ]] && { echo "$id"; return 0; }
		sleep 0.2
	done
	return 1
}

echo "--- starting both nodes ---"
PID_A=$(start a); PID_B=$(start b)
wait_for_socket a; wait_for_socket b

echo "--- A creates a network, B joins ---"
on a create filetest >/dev/null
CODE=$(on a invite | awk '/invite code/ {print $NF}')
on b join "$CODE" >/dev/null || { echo "join failed" >&2; exit 1; }

# The daemons read settings once at startup, so the operator is set and both
# are restarted — the state an installed node is in.
on a set-operator "$SUDO_USER" >/dev/null
on b set-operator "$SUDO_USER" >/dev/null
kill "$PID_A" "$PID_B"; wait "$PID_A" "$PID_B" 2>/dev/null
PID_A=$(start a); PID_B=$(start b)
wait_for_socket a; wait_for_socket b

B_ID=$(on b status | awk '/^ID/ {print $NF}')
B_V4=$(on b status | awk '/^IPv4/ {print $NF}')
B_FALLBACK=${B_ID:0:8}
echo "B is $B_FALLBACK, $B_V4"

echo; echo "=== 1. a 256 MiB file, addressed by overlay IP, accepted with --here ==="
sudo -u "$SUDO_USER" head -c $((256 * 1024 * 1024)) /dev/urandom >"$WORK/a/big.bin"
chown "$SUDO_USER" "$WORK/a/big.bin"
as_user a "$WORK/a" file send big.bin "$B_V4" >"$ROOT/send1.log" 2>&1 &
SENDER=$!
if ID=$(wait_for_offer_on_b); then
	pass "the offer is listed on B (id $ID)"
	START=$(date +%s.%N)
	if as_user b "$WORK/b" file accept "$ID" --here; then
		END=$(date +%s.%N)
		pass "accept finished ($(echo "256 / ($END - $START)" | bc -l | cut -c1-6) MiB/s)"
	else
		fail "accept failed"
	fi
	if wait "$SENDER"; then pass "the sender reports delivered"; else fail "the sender exited $?: $(cat "$ROOT/send1.log")"; fi
	if cmp -s "$WORK/a/big.bin" "$WORK/b/big.bin"; then pass "the received file is identical"; else fail "the received file differs"; fi
	OWNER=$(stat -c %U "$WORK/b/big.bin" 2>/dev/null)
	[[ $OWNER == "$SUDO_USER" ]] && pass "owned by $SUDO_USER, not root" || fail "owned by '$OWNER'"
	ls -A "$WORK/b" | grep -q '\.part$' && fail "a .part file was left behind" || pass "no .part file left behind"
	as_user a "$WORK/a" file list | grep -q "done" && pass "A lists it as done" || fail "A does not list it as done"
else
	fail "the offer never appeared on B"; kill "$SENDER" 2>/dev/null
fi

echo; echo "=== 2. a rejection, addressed by fallback id ==="
echo "no thanks" | sudo -u "$SUDO_USER" tee "$WORK/a/small.txt" >/dev/null
as_user a "$WORK/a" file send small.txt "$B_FALLBACK" >"$ROOT/send2.log" 2>&1 &
SENDER=$!
if ID=$(wait_for_offer_on_b); then
	as_user b "$WORK/b" file reject "$ID" >/dev/null && pass "rejected on B" || fail "reject failed"
	wait "$SENDER"; CODE=$?
	[[ $CODE -eq 3 ]] && pass "the sender exits 3 (rejected)" || fail "the sender exited $CODE"
	[[ ! -e $WORK/b/small.txt ]] && pass "nothing was written" || fail "a rejected file was written"
else
	fail "the offer never appeared on B"; kill "$SENDER" 2>/dev/null
fi

echo; echo "=== 3. the sender cancels while waiting ==="
as_user a "$WORK/a" file send small.txt "$B_FALLBACK" >"$ROOT/send3.log" 2>&1 &
SENDER=$!
if ID=$(wait_for_offer_on_b); then
	# The CLI runs under sudo; signal the quix process itself, as Ctrl+C would.
	pkill -INT -u "$SUDO_USER" -f "$QUIX file send small.txt" || true
	wait "$SENDER"; CODE=$?
	[[ $CODE -eq 130 ]] && pass "the sender exits 130 (cancelled)" || fail "the sender exited $CODE"
	GONE=""
	for _ in $(seq 1 50); do [[ -z $(first_offer_on_b) ]] && { GONE=1; break; }; sleep 0.1; done
	[[ -n $GONE ]] && pass "the offer disappeared from B" || fail "the offer is still listed on B"
	MSG=$(as_user b "$WORK/b" file accept "$ID" --here 2>&1)
	echo "$MSG" | grep -q "no longer available" && pass "accepting it now says the sender is gone" || fail "accept said: $MSG"
else
	fail "the offer never appeared on B"; kill "$SENDER" 2>/dev/null
fi

echo; echo "=== 4. the receiver dies mid-transfer ==="
mkdir -p "$WORK/b/partial"; chown "$SUDO_USER" "$WORK/b/partial"
as_user a "$WORK/a" file send big.bin "$B_FALLBACK" >"$ROOT/send4.log" 2>&1 &
SENDER=$!
if ID=$(wait_for_offer_on_b); then
	as_user b "$WORK/b/partial" file accept "$ID" --here >/dev/null 2>&1 &
	RECEIVER=$!
	sleep 0.4
	pkill -KILL -u "$SUDO_USER" -f "$QUIX file accept $ID" || true
	wait "$RECEIVER" 2>/dev/null
	wait "$SENDER"; CODE=$?
	if [[ -e $WORK/b/partial/big.bin ]]; then
		echo "  SKIP  the transfer finished inside 0.4s; nothing to interrupt"
	else
		[[ $CODE -eq 1 ]] && pass "the sender reports failed" || fail "the sender exited $CODE: $(cat "$ROOT/send4.log")"
		# SIGKILL gives the receiver no chance to clean up: this is the one
		# case that can leave a .part file, and the check says so.
		PARTS=$(ls -A "$WORK/b/partial" | grep -c '\.part$')
		echo "  NOTE  SIGKILLed receiver left $PARTS .part file(s) (expected: at most 1)"
	fi
else
	fail "the offer never appeared on B"; kill "$SENDER" 2>/dev/null
fi

echo; echo "=== 5. a file the user cannot read ==="
MSG=$(as_user a "$WORK/a" file send /etc/shadow "$B_FALLBACK" 2>&1); CODE=$?
[[ $CODE -ne 0 ]] && echo "$MSG" | grep -q "Permission denied" \
	&& pass "refused with the OS error" || fail "exit $CODE: $MSG"
[[ -z $(first_offer_on_b) ]] && pass "nothing was offered" || fail "an offer reached B"

if [[ ${EXPIRY:-0} == 1 ]]; then
	echo; echo "=== 6. an offer nobody answers (about a minute) ==="
	as_user a "$WORK/a" file send small.txt "$B_FALLBACK" --expires 1 min >"$ROOT/send6.log" 2>&1 &
	SENDER=$!
	wait "$SENDER"; CODE=$?
	[[ $CODE -eq 4 ]] && pass "the sender exits 4 (expired)" || fail "the sender exited $CODE"
	[[ -z $(first_offer_on_b) ]] && pass "gone from B" || fail "still listed on B"
fi

echo; echo "=== $PASSED passed, $FAILED failed ==="
[[ $FAILED -eq 0 ]]
