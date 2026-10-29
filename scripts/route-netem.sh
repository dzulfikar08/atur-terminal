#!/usr/bin/env bash
# Delays loopback traffic to and from 127.0.0.2 and nothing else, so one address of a
# server is slow while 127.0.0.1 stays fast (the `muxy-client` route race tests).
#
#   route-netem.sh on    120 ms delay each way for packets whose destination or source is 127.0.0.2
#   route-netem.sh off   remove the shaping
#
# Needs root for tc; uses sudo when it is not.
set -euo pipefail

DELAY="${ATUR_ROUTE_DELAY:-120ms}"
root() { if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo "$@"; fi; }

case "${1:-}" in
  on)
    root tc qdisc replace dev lo root handle 1: prio
    root tc qdisc replace dev lo parent 1:3 handle 30: netem delay "$DELAY"
    # A packet to the address and a packet from it, whichever source the kernel picked.
    for side in dst src; do
      root tc filter add dev lo parent 1: protocol ip prio 1 u32 \
        match ip "$side" 127.0.0.2/32 flowid 1:3
    done
    ;;
  off) root tc qdisc del dev lo root 2>/dev/null || true ;;
  *)
    echo "usage: route-netem.sh on|off" >&2
    exit 2
    ;;
esac
