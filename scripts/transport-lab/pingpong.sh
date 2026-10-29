#!/bin/bash
# Round trip of a keystroke-sized message over TCP, QUIC and the Atur Terminal
# UDP stream on the lab's emulated links (see run.sh for the method).
#
#   scripts/transport-lab/pingpong.sh [COUNT] [SCENARIO...]
#
# Build first: (cd scripts/transport-lab/pingpong && cargo build --release)
set -euo pipefail

if [ -z "${ATUR_LAB_INSIDE:-}" ]; then
    here=$(cd "$(dirname "$0")" && pwd)
    export ATUR_LAB_INSIDE=1 ATUR_LAB_SCRIPT="$here/pingpong.sh"
    exec unshare --user --map-root-user --net -- "$ATUR_LAB_SCRIPT" "$@"
fi

here=$(cd "$(dirname "$0")" && pwd)
BIN=${PINGPONG:-$here/pingpong/target/release/pingpong}
COUNT=${1:-200}
shift || true
SCENARIOS=("$@")
[ ${#SCENARIOS[@]} -gt 0 ] || SCENARIOS=(rtt50 rtt300 rtt800)

ip link set lo up
ip link set dev lo mtu 1500
ip link set dev lo gso_max_size 1514 gso_max_segs 1 2>/dev/null || true

port=45000
for scenario in "${SCENARIOS[@]}"; do
    case $scenario in
        rtt50)  shape="delay 25ms" ;;
        rtt300) shape="delay 150ms 30ms loss 5%" ;;
        rtt800) shape="delay 400ms 200ms loss 10%" ;;
        *) echo "unknown scenario $scenario" >&2; exit 2 ;;
    esac
    for transport in tcp quic udp; do
        tc qdisc replace dev lo root netem $shape
        port=$((port + 1))
        run=$(timeout 900 "$BIN" "$transport" "$port" "$COUNT" 2>&1 | tail -1) || run='{"error":"failed"}'
        echo "{\"scenario\":\"$scenario\",\"run\":$run}"
    done
done
