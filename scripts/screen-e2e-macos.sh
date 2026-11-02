#!/usr/bin/env bash
# Checks a Mac as a screen source end to end through the real server: the server
# starts the screen program (atur-term-screen) itself, a client opens a screen on
# the server's channel and watches it with the real viewer.
#
#  1. A moving test pattern in place of the screen (ATUR_TERM_SCREEN_SYNTHETIC):
#     no permission needed, so this always runs. The pictures are H.264 from the
#     Mac's VideoToolbox encoder, decoded by the viewer's VideoToolbox decoder.
#  2. The real screen. A machine where Screen Recording is not allowed to the
#     program (a laptop that has not been set up) must say so; a machine where
#     it is allowed must show the screen. Either outcome passes, and which one
#     it was is printed.
#
# Run by the `macos` job of .github/workflows/ci.yml, or by hand on a Mac:
#   scripts/screen-e2e-macos.sh [TARGET_DIR]   # default: target/debug, built here
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${1:-$ROOT/target/debug}
SUMMARY=${GITHUB_STEP_SUMMARY:-/dev/null}
cd "$ROOT"
cargo build --locked -p muxy-server -p muxy-cli -p atur-screen-host --bins --example screen_probe
PROBE="$BIN/examples/screen_probe"

WORK=$(mktemp -d /tmp/atur-e2e.XXXXXX)
SERVER_PID=""
cleanup() {
  status=$?
  [[ -n "$SERVER_PID" ]] && kill "$SERVER_PID" 2>/dev/null || true
  if [[ $status -ne 0 ]]; then
    echo "::group::the server's log"
    cat "$WORK/server.log" "$WORK/srv/server.log" 2>/dev/null || true
    echo "::endgroup::"
  fi
  rm -rf "$WORK"
  exit $status
}
trap cleanup EXIT

start_server() { # start_server [VAR=value]...
  mkdir -p "$WORK/srv"
  env ATUR_TERM_DIR="$WORK/srv" ATUR_TERM_REMOTE_BIND=127.0.0.1 SHELL=/bin/sh \
    ATUR_TERM_SCREEN_HELPER="$BIN/atur-term-screen" "$@" \
    "$BIN/atur-term-server" >"$WORK/server.log" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 200); do
    [[ -S "$WORK/srv/server.sock" ]] && return 0
    sleep 0.1
  done
  echo "the server did not start" >&2
  return 1
}

stop_server() {
  kill "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
  SERVER_PID=""
  rm -rf "$WORK/srv"
}

echo "== the test pattern, through the server"
start_server ATUR_TERM_SCREEN_SYNTHETIC=640x360
PATTERN=$("$PROBE" --server "$WORK/srv/server.sock" fps 4)
echo "$PATTERN"
echo "$PATTERN" | grep -q '"codec":"h264 (videotoolbox)"'
python3 - "$PATTERN" <<'PY'
import json, sys
report = json.loads(sys.argv[1])
assert report["fps_mean"] >= 10, f"the pattern reached the viewer at {report['fps_mean']} pictures a second"
PY
stop_server

echo "== the real screen, through the server"
start_server
if REAL=$("$PROBE" --server "$WORK/srv/server.sock" fps 2 2>"$WORK/real.err"); then
  echo "shown: $REAL"
  echo "$REAL" | grep -q '"codec":"h264'
  OUTCOME="the real screen was shared: $REAL"
else
  cat "$WORK/real.err"
  grep -q "Screen recording permission needed" "$WORK/real.err" || grep -q "no display" "$WORK/real.err"
  OUTCOME="skipped the capture: $(head -c 400 "$WORK/real.err")"
fi
{
  echo "### Mac screen source through the server"
  echo
  echo '```json'
  echo "$PATTERN"
  echo '```'
  echo
  echo "Real screen: $OUTCOME"
} >>"$SUMMARY"
