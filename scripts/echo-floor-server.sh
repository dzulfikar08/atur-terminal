#!/bin/bash
# Starts a standalone server in its own profile, with phone access on a port, and leaves it running
# with a pairing link for <address> (one use, five minutes) written to <dir>/link.
#
#   echo-floor-server.sh <bin-dir> <mobile-port> <address> <dir>
set -euo pipefail

bin="$(cd "$1" && pwd)"
port="$2"
address="$3"
dir="$4"
mkdir -p "$dir/profile"
dir="$(cd "$dir" && pwd)"
export ATUR_TERM_DIR="$dir/profile"
export ATUR_TERM_UDP_PORTS="${ATUR_TERM_UDP_PORTS:-60096-60103}"
export SHELL=/bin/bash

nohup "$bin/atur-term-server" > "$dir/server.log" 2>&1 &
for _ in $(seq 150); do [ -S "$ATUR_TERM_DIR/server.sock" ] && break; sleep 0.1; done
[ -S "$ATUR_TERM_DIR/server.sock" ] || { echo "the server did not start" >&2; cat "$dir/server.log" >&2; exit 1; }
"$bin/atur-term" mobile enable --port "$port"
nohup "$bin/atur-term" mobile pair --address "$address" > "$dir/pair.log" 2>&1 &
for _ in $(seq 120); do grep -qE '^[a-z-]+://pair\?' "$dir/pair.log" && break; sleep 0.25; done
grep -E '^[a-z-]+://pair\?' "$dir/pair.log" | head -1 > "$dir/link"
[ -s "$dir/link" ] || { echo "no pairing link; the server said:" >&2; cat "$dir/pair.log" >&2; exit 1; }
echo "server running, link in $dir/link"
