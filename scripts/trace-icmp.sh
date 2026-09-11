#!/usr/bin/env bash
# Traces an inbound ping through nftables to find the rule that drops it.
#
#   sudo ./scripts/trace-icmp.sh          # then ping this host from the peer
#
# Prints every chain and rule each echo-request traverses. The last line before
# the packet disappears is the rule responsible. Removes its own table on exit.
set -euo pipefail

IFACE=${QUIX_IFACE:-quix}
TABLE=quixtrace

[[ $EUID -eq 0 ]] || { echo "needs root: sudo $0" >&2; exit 1; }

cleanup() {
	nft delete table inet "$TABLE" 2>/dev/null || true
	echo
	echo "--- trace table removed ---"
}
trap cleanup EXIT

nft delete table inet "$TABLE" 2>/dev/null || true
nft add table inet "$TABLE"
# Priority -300 puts this ahead of conntrack and every normal filter chain, so
# the packet is marked for tracing before anything has a chance to drop it.
nft add chain inet "$TABLE" pre "{ type filter hook prerouting priority -300 ; }"
nft add rule inet "$TABLE" pre iifname "$IFACE" icmp type echo-request meta nftrace set 1

echo "tracing echo-requests arriving on $IFACE — ping this host now (ctrl-c to stop)"
echo
nft monitor trace
