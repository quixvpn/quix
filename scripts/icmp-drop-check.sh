#!/usr/bin/env bash
# Samples the kernel's IP/ICMP counters around a burst of inbound pings, so the
# layer that swallows them is visible as a counter that does NOT move.
#
#   sudo ./scripts/icmp-drop-check.sh          # then ping this host from the peer
#
# Reading the deltas:
#   IpInReceives  moves, IpInDelivers doesn't  -> dropped at IP input (checksum,
#                                                 martian source, no local route)
#   IpInDelivers  moves, IcmpInEchos doesn't   -> dropped by netfilter (firewall)
#   IcmpInEchos   moves, IcmpOutEchoReps doesn't -> the kernel saw it and chose
#                                                 not to answer (sysctl/ICMP policy)
#   IcmpOutEchoReps moves                      -> we DID reply; the loss is on the
#                                                 way back, not here
set -euo pipefail

WINDOW=${1:-20}
KEYS='IpInReceives|IpInDelivers|IpInHdrErrors|IpInAddrErrors|IpInDiscards|IcmpInMsgs|IcmpInEchos|IcmpInCsumErrors|IcmpOutEchoReps'

snapshot() { nstat -az 2>/dev/null | grep -wE "$KEYS" | awk '{print $1, $2}' | sort; }

echo "sampling for ${WINDOW}s — send pings to this host now"
before=$(snapshot)
sleep "$WINDOW"
after=$(snapshot)

echo
printf '%-20s %12s\n' "COUNTER" "DELTA"
join <(echo "$before") <(echo "$after") | while read -r name old new; do
	delta=$((new - old))
	[[ $delta -ne 0 ]] && printf '%-20s %12s\n' "$name" "+$delta"
done

echo
echo "(counters absent above did not move at all)"

# A missing IcmpInEchos is ambiguous — no pings sent looks identical to pings
# dropped before ICMP — so say which case needs ruling out.
echoes=$(join <(echo "$before") <(echo "$after") |
	awk '$1 == "IcmpInEchos" { print $3 - $2 }')
if [[ ${echoes:-0} -eq 0 ]]; then
	echo
	echo "WARNING: no echo requests reached the ICMP layer."
	echo "Either none were sent during the window, or they are dropped before ICMP."
	echo "Re-run with a CONTINUOUS ping already in flight (Windows: ping -t <ip>)."
fi
