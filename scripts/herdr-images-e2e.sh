#!/usr/bin/env bash
# Checks that Kitty images of a herdr pane show on an Atur Terminal client with
# the real herdr, and again after a restart of atur-term-server: a program in the
# pane prints images the way `kitty +kitten icat` does (kitty_image.py), and the
# probe, a client of the server, requires them on its screen, with the right
# pixels and cells, scrolled, resized and deleted.
#
#   scripts/herdr-images-e2e.sh BIN_DIR HERDR   # BIN_DIR holds atur-term-server and
#                                               # herdr_images_probe
set -euo pipefail

BIN=$(cd "${1:?usage: herdr-images-e2e.sh BIN_DIR HERDR}" && pwd)
HERDR=$(readlink -f "${2:?usage: herdr-images-e2e.sh BIN_DIR HERDR}")
SCRIPT=$(cd "$(dirname "$0")" && pwd)/kitty_image.py
WORK=$(mktemp -d /tmp/atur-img.XXXXXX)
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
    echo "::group::herdr's own log"; cat "$XDG_CONFIG_HOME"/herdr/herdr-server.log 2>/dev/null || true; echo "::endgroup::"
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
python3 --version
"$HERDR" server >"$WORK/herdr.log" 2>&1 &
wait_for "herdr's API socket" test -S "$HERDR_SOCKET_PATH"
wait_for "herdr's client socket" test -S "$WORK/herdr-client.sock"

mkdir -p "$WORK/srv"
start_server() { # start_server LOGNAME
  ATUR_TERM_DIR="$WORK/srv" ATUR_TERM_BACKEND=herdr ATUR_TERM_HERDR_BIN="$HERDR" \
    ATUR_TERM_HERDR_SOCKET="$HERDR_SOCKET_PATH" "$BIN/atur-term-server" >"$WORK/$1.log" 2>&1 &
  SERVER_PID=$!
  wait_for "the server socket" test -S "$WORK/srv/server.sock"
}

echo "== print images through the first server"
start_server server1
"$BIN/herdr_images_probe" "$WORK/srv/server.sock" "$SCRIPT" seed

echo "== kill -9 the server, start another"
kill -9 "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
rm -f "$WORK/srv/server.sock"
start_server server2
"$BIN/herdr_images_probe" "$WORK/srv/server.sock" "$SCRIPT" check
echo "images of a herdr pane: shown, moved, resized, deleted, and again after a restart"
