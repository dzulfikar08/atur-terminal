#!/usr/bin/env bash
# Checks that prompt marks survive a restart of atur-term-server on an isolated
# herdr: submit commands through the server, kill -9 the server, start another
# one, and require every mark again on the adopted session.
#
#   scripts/herdr-marks-e2e.sh BIN_DIR HERDR   # BIN_DIR holds atur-term-server and
#                                              # herdr_marks_probe
set -euo pipefail

BIN=$(cd "${1:?usage: herdr-marks-e2e.sh BIN_DIR HERDR}" && pwd)
HERDR=$(readlink -f "${2:?usage: herdr-marks-e2e.sh BIN_DIR HERDR}")
COUNT=12
WORK=$(mktemp -d)
export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/config" XDG_RUNTIME_DIR="$WORK/run"
mkdir -m 700 "$HOME" "$XDG_CONFIG_HOME" "$XDG_RUNTIME_DIR"
export HERDR_SOCKET_PATH="$WORK/herdr.sock"
export SHELL=/bin/sh ENV= PS1='$ '

cleanup() {
  status=$?
  if [[ $status -ne 0 ]]; then
    for log in herdr server1 server2; do
      echo "::group::$log log"; cat "$WORK/$log.log" 2>/dev/null || true; echo "::endgroup::"
    done
    echo "::group::herdr.json"; find "$WORK/srv" -name herdr.json -exec cat {} + 2>/dev/null || true; echo "::endgroup::"
    echo "::group::the server's own log"; cat "$WORK/srv/server.log" 2>/dev/null || true; echo "::endgroup::"
  fi
  kill $(jobs -p) 2>/dev/null || true
  "$HERDR" server stop >/dev/null 2>&1 || true
  wait 2>/dev/null || true
  rm -rf "$WORK"
  exit $status
}
trap cleanup EXIT

wait_for() {
  local what=$1; shift
  for _ in $(seq 1 200); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.1
  done
  echo "timed out waiting for $what" >&2
  return 1
}

"$HERDR" --version
"$HERDR" server >"$WORK/herdr.log" 2>&1 &
wait_for "herdr's API socket" test -S "$HERDR_SOCKET_PATH"

mkdir -p "$WORK/srv"
start_server() { # start_server LOGNAME
  ATUR_TERM_DIR="$WORK/srv" ATUR_TERM_BACKEND=herdr ATUR_TERM_HERDR_BIN="$HERDR" \
    ATUR_TERM_HERDR_SOCKET="$HERDR_SOCKET_PATH" "$BIN/atur-term-server" >"$WORK/$1.log" 2>&1 &
  SERVER_PID=$!
  wait_for "the server socket" test -S "$WORK/srv/server.sock"
}

echo "== submit $COUNT commands through the first server"
start_server server1
"$BIN/herdr_marks_probe" "$WORK/srv/server.sock" seed "$COUNT"

echo "== the saved marks"
STORE=$(find "$WORK/srv" -name herdr.json | head -1)
echo "$(grep -c '"row"' "$STORE") marks in $STORE"
test "$(grep -c '"row"' "$STORE")" -eq "$COUNT"

echo "== kill -9 the server, start another"
kill -9 "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
rm -f "$WORK/srv/server.sock"
start_server server2
"$BIN/herdr_marks_probe" "$WORK/srv/server.sock" check "$COUNT"
echo "prompt marks after a restart: $COUNT/$COUNT"
