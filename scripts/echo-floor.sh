#!/bin/bash
# Measures the phone core's echo latency against a standalone server process (the executable,
# started in its own profile), not the in-process server of the facade tests.
#
#   echo-floor.sh <label> <bin-dir> <mobile-port> [variants]
#
# <bin-dir> holds atur-term and atur-term-server. The server runs with an isolated ATUR_TERM_DIR,
# phone access on <mobile-port> and UDP offers on 60096-60103; the bench pairs once with the link the
# command-line client prints and then connects once per variant (see bench_facade_external).
# ECHO_FLOOR_LOAD=<n> keeps n busy processes running meanwhile; ECHO_FLOOR_PROXY=1 puts a plain
# socat TCP forwarder (Nagle on, like an emulator's user-mode network) between phone and server.
set -euo pipefail

label="$1"
bin="$(cd "$2" && pwd)"
port="$3"
variants="${4:-core,tls,tls+sessions,tls+sessions+history,auto,auto+sessions}"

dir="$(mktemp -d)"
mkdir -p "$dir/profile"
export ATUR_TERM_DIR="$dir/profile"
export ATUR_TERM_UDP_PORTS="${ATUR_TERM_UDP_PORTS:-60096-60103}"
export SHELL=/bin/bash

chmod +x "$bin/atur-term" "$bin/atur-term-server"
nohup "$bin/atur-term-server" > "$dir/server.log" 2>&1 &
server=$!
burners=()
cleanup() {
  kill "$server" 2>/dev/null || true
  for pid in "${burners[@]:-}" "${proxy:-}"; do kill "$pid" 2>/dev/null || true; done
  kill "${pairing:-0}" 2>/dev/null || true
  cp "$dir/server.log" "server-$label.log" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 150); do [ -S "$ATUR_TERM_DIR/server.sock" ] && break; sleep 0.1; done
if [ ! -S "$ATUR_TERM_DIR/server.sock" ]; then
  echo "the server did not start" >&2
  ls -la "$ATUR_TERM_DIR" >&2 || true
  cat "$dir/server.log" >&2
  exit 1
fi
"$bin/atur-term" mobile enable --port "$port"

nohup "$bin/atur-term" mobile pair --address 127.0.0.1 > "$dir/pair.log" 2>&1 &
pairing=$!
for _ in $(seq 120); do grep -qE '^[a-z-]+://pair\?' "$dir/pair.log" && break; sleep 0.25; done
grep -E '^[a-z-]+://pair\?' "$dir/pair.log" | head -1 > "$dir/link"
if [ ! -s "$dir/link" ]; then
  echo "no pairing link from the server; its answer was:" >&2
  cat "$dir/pair.log" >&2
  exit 1
fi

if [ -n "${ECHO_FLOOR_PROXY:-}" ]; then
  socat "TCP-LISTEN:$((port + 100)),reuseaddr,fork" "TCP:127.0.0.1:$port" &
  proxy=$!
  export ATUR_BENCH_PORT=$((port + 100))
  sleep 0.5
fi
for _ in $(seq "${ECHO_FLOOR_LOAD:-0}"); do
  yes > /dev/null &
  burners+=($!)
done

echo "== $label: $(nproc) cpus, $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2)"
ATUR_BENCH_LINK_FILE="$dir/link" ATUR_BENCH_LABEL="$label" ATUR_BENCH_VARIANTS="$variants" \
  cargo test --release --locked -p atur-mobile --test sdk bench_facade_external -- --ignored --nocapture 2>&1 \
  | tee "bench-$label.txt"
