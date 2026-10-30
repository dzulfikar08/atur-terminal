#!/usr/bin/env bash
# Checks screen mirroring end to end on a headless sway with the real wayvnc:
# the server starts wayvnc itself, a client opens a screen over the server's
# channel, sees the desktop change, drives it with the pointer and keyboard and
# exchanges clipboard text. The same changes are timed straight against
# wayvnc, to show what the relay adds. Then the phone core does the same
# through the facade the phone apps use: it pairs over TLS, is refused the
# screen until the computer allows it, and over TLS and over UDP sees the
# desktop change, drives it, and loses the screen when the permission goes.
#
# Needs sway, wayvnc, wev, wl-clipboard and python. Run by the `screen-e2e` job of
# .github/workflows/ci.yml with the binaries built by the `screen-build` job:
#   scripts/screen-e2e.sh BIN_DIR   # holds atur-term-server, screen_probe and phone_probe
set -euo pipefail

BIN=$(cd "${1:?usage: screen-e2e.sh BIN_DIR}" && pwd)
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
export XDG_RUNTIME_DIR="$WORK/run"
export HOME="$WORK/home"
mkdir -m 700 "$XDG_RUNTIME_DIR" "$HOME"
export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman
SUMMARY=${GITHUB_STEP_SUMMARY:-/dev/null}

cleanup() {
  status=$?
  if [[ $status -ne 0 ]]; then
    for log in sway server wayvnc-direct wev; do
      echo "::group::$log log"
      cat "$WORK/$log.log" 2>/dev/null || true
      echo "::endgroup::"
    done
    echo "::group::the server's own log"
    cat "$WORK/srv/server.log" 2>/dev/null || true
    echo "::endgroup::"
  fi
  kill $(jobs -p) 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf "$WORK"
  exit $status
}
trap cleanup EXIT

wait_for() { # wait_for DESCRIPTION COMMAND...
  local what=$1; shift
  for _ in $(seq 1 200); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.1
  done
  echo "timed out waiting for $what" >&2
  return 1
}

echo "wayvnc: $(wayvnc --version 2>&1 | head -1); sway: $(sway --version 2>&1 | head -1)"

sway --unsupported-gpu -c "$HERE/screen-e2e/sway.conf" >"$WORK/sway.log" 2>&1 &
wait_for "the sway socket" bash -c 'ls "$XDG_RUNTIME_DIR"/wayland-[0-9]'
export WAYLAND_DISPLAY=$(basename "$(ls "$XDG_RUNTIME_DIR"/wayland-[0-9] | head -1)")
wait_for "sway's IPC socket" bash -c 'ls "$XDG_RUNTIME_DIR"/sway-ipc.*.sock'
export SWAYSOCK=$(ls "$XDG_RUNTIME_DIR"/sway-ipc.*.sock | head -1)
wait_for "an output" bash -c 'swaymsg -t get_outputs | grep -q HEADLESS'

# What a user's server does: start wayvnc itself, on demand.
mkdir -p "$WORK/srv"
ATUR_TERM_DIR="$WORK/srv" ATUR_TERM_SCREEN_GPU=0 ATUR_TERM_REMOTE_BIND=127.0.0.1 SHELL=/bin/sh \
  "$BIN/atur-term-server" >"$WORK/server.log" 2>&1 &
wait_for "the server socket" test -S "$WORK/srv/server.sock"
SERVER=(--server "$WORK/srv/server.sock")

# A second wayvnc of our own, to time without the relay.
wayvnc -r -f 60 -L debug -S "$WORK/direct.ctl" "unix:$WORK/direct.sock" >"$WORK/wayvnc-direct.log" 2>&1 &
wait_for "the direct wayvnc socket" test -S "$WORK/direct.sock"
DIRECT=(--direct "$WORK/direct.sock")

probe() { "$BIN/screen_probe" "$@"; }

echo "== the desktop's colour reaches the client"
swaymsg output '*' bg '#c81e1e' solid_color
probe "${SERVER[@]}" expect 200 30 30
swaymsg output '*' bg '#1e1ec8' solid_color
probe "${SERVER[@]}" expect 30 30 200

echo "== how long a change takes to show: through the server, and straight to wayvnc"
RELAY=$(probe "${SERVER[@]}" colors 30)
DIRECT_RESULT=$(probe "${DIRECT[@]}" colors 30)
echo "$RELAY"
echo "$DIRECT_RESULT"
{
  echo "### Screen mirroring, headless sway, wayvnc $(wayvnc --version 2>&1 | head -1)"
  echo
  echo '```json'
  echo "$RELAY"
  echo "$DIRECT_RESULT"
  echo '```'
} >>"$SUMMARY"

# The phone core: the facade the phone apps drive, paired like a phone, with the
# permission a phone needs from the computer.
phone() { "$BIN/phone_probe" --server "$WORK/srv/server.sock" --credential "$WORK/phone.json" "$@"; }

echo "== a phone pairs, and is refused the screen until the computer allows it"
phone pair
phone refused
phone allow

echo "== a phone sees the desktop's colour over TLS and over UDP, with and without H.264 offered"
PHONE_REPORT=""
for transport in tls auto; do
  for h264 in off on; do
    PHONE_REPORT+="$(phone --expect-udp colors "$transport" "$h264")"$'\n'
  done
done
printf '%s' "$PHONE_REPORT"
{
  echo "### Phone core: the screen through the facade, TLS then UDP, H.264 offered or not"
  echo
  echo '```json'
  printf '%s' "$PHONE_REPORT"
  echo '```'
} >>"$SUMMARY"

# A window to receive the keys and clicks; it fills the output, so it comes after
# the checks that read the background's colour.
stdbuf -oL wev >"$WORK/wev.log" 2>&1 &
wait_for "the wev window" bash -c 'swaymsg -t get_tree | grep -q "\"app_id\": \"wev\""'

echo "== keys, clicks and the wheel reach the desktop"
probe "${SERVER[@]}" input
sleep 0.5
grep -E "sym: a " "$WORK/wev.log"
grep -E "sym: B " "$WORK/wev.log"
grep -E "wl_pointer\] button: .*state: 1" "$WORK/wev.log"
grep -E "wl_pointer\] axis" "$WORK/wev.log"

echo "== a phone's keys, clicks and the wheel reach the desktop"
for transport in tls auto; do
  seen_lines=$(wc -l <"$WORK/wev.log")
  phone --expect-udp input "$transport"
  sleep 0.5
  tail -n +"$((seen_lines + 1))" "$WORK/wev.log" >"$WORK/wev-phone.log"
  grep -E "sym: x " "$WORK/wev-phone.log"
  grep -E "sym: Y " "$WORK/wev-phone.log"
  grep -E "wl_pointer\] button: .*state: 1" "$WORK/wev-phone.log"
  grep -E "wl_pointer\] axis" "$WORK/wev-phone.log"
done

echo "== taking the permission away ends a phone's screen at once"
phone permission

echo "== the clipboard, both ways"
# wayvnc holds the desktop's selection for the client that set it and drops it
# when that client disconnects, so the client stays connected while the desktop is read.
paste() { wl-paste --no-newline 2>"$WORK/wl-paste.err" || true; }
for via in SERVER DIRECT; do
  declare -n target=$via
  wanted="héllo wörld via $via"
  (probe "${target[@]}" clipboard-out "$wanted" 8 >"$WORK/clipboard-out.json") &
  sender=$!
  for _ in $(seq 1 50); do
    [[ "$(paste)" == "$wanted" ]] && break
    sleep 0.1
  done
  if [[ "$(paste)" != "$wanted" ]]; then
    echo "the desktop clipboard did not receive the client's text ($via): '$(paste)' $(cat "$WORK/wl-paste.err")" >&2
    echo "::group::wayvnc (direct) log"; tail -150 "$WORK/wayvnc-direct.log"; echo "::endgroup::"
    echo "::group::the server's own log"; tail -50 "$WORK/srv/server.log"; echo "::endgroup::"
    exit 1
  fi
  echo "viewer to desktop ($via): ok"
  wait "$sender"
  (probe "${target[@]}" clipboard-in "from the desktop via $via" >"$WORK/clipboard-in.json") &
  reader=$!
  sleep 2
  printf 'from the desktop via %s' "$via" | wl-copy
  wait "$reader"
  echo "desktop to viewer ($via): ok"
done

echo "== the capture stops with the last viewer, and with the server"
pgrep -f "wayvnc .*atur-term-screen" >/dev/null
kill -TERM "$(pgrep -f atur-term-server | head -1)"
for _ in $(seq 1 50); do
  pgrep -f "wayvnc .*atur-term-screen" >/dev/null || break
  sleep 0.2
done
if pgrep -f "wayvnc .*atur-term-screen" >/dev/null; then
  echo "the server's wayvnc outlived the server" >&2
  exit 1
fi
echo "screen mirroring end to end: ok"
