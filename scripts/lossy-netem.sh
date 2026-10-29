#!/usr/bin/env bash
# Shapes loopback traffic of the facade bench (`bench_facade_lossy`) and nothing else.
#
#   lossy-netem.sh on          400 ms one-way delay, 200 ms jitter and 5% loss each way (about
#                              800 ms round trip, 10% round-trip loss) for the two ports below
#   lossy-netem.sh outage-on   drop every packet of those ports
#   lossy-netem.sh outage-off  let them through again
#   lossy-netem.sh off         remove the shaping
#
# Ports: the bench's TLS listener (ATUR_BENCH_TLS_PORT) and the single UDP port the server may
# offer (ATUR_TERM_UDP_PORTS, one port, as in `60100`). Both directions match: a packet either
# goes to the port or comes from it. Needs root for tc and iptables; uses sudo when it is not.
set -euo pipefail

: "${ATUR_BENCH_TLS_PORT:?set ATUR_BENCH_TLS_PORT to the TLS port the bench listens on}"
: "${ATUR_TERM_UDP_PORTS:?set ATUR_TERM_UDP_PORTS to one UDP port}"
TLS_PORT="$ATUR_BENCH_TLS_PORT"
UDP_PORT="$ATUR_TERM_UDP_PORTS"
case "$UDP_PORT" in *[!0-9]*) echo "ATUR_TERM_UDP_PORTS must be a single port" >&2; exit 2 ;; esac

root() { if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo "$@"; fi; }

# Every rule the outage adds, as arguments for `iptables -A/-D OUTPUT`.
outage_rules() {
  local action="$1"
  for protocol_port in "tcp $TLS_PORT" "udp $UDP_PORT"; do
    set -- $protocol_port
    for side in --dport --sport; do
      # Removing a rule that is not there is fine: cleanup may run after a failed test.
      root iptables "$action" OUTPUT -o lo -p "$1" "$side" "$2" -j DROP || [ "$action" = -D ]
    done
  done
}

case "${1:-}" in
  on)
    root tc qdisc replace dev lo root handle 1: prio
    root tc qdisc replace dev lo parent 1:3 handle 30: netem delay 400ms 200ms loss 5%
    for protocol_port in "6 $TLS_PORT" "17 $UDP_PORT"; do
      set -- $protocol_port
      for side in dport sport; do
        root tc filter add dev lo parent 1: protocol ip prio 1 u32 \
          match ip protocol "$1" 0xff match ip "$side" "$2" 0xffff flowid 1:3
      done
    done
    ;;
  outage-on) outage_rules -I ;;
  outage-off) outage_rules -D ;;
  off) root tc qdisc del dev lo root 2>/dev/null || true ;;
  *)
    echo "usage: lossy-netem.sh on|outage-on|outage-off|off" >&2
    exit 2
    ;;
esac
