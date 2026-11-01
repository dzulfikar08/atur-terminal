#!/usr/bin/env bash
# Measures how many pictures a second a screen viewer receives over the UDP
# transport on links with a long round trip, with the real wayvnc on a headless
# sway and something on the desktop that moves 60 times a second.
#
# The viewer, the bridge and the server run on this computer; the UDP leg goes
# over the loopback interface, which `tc netem` delays by half the round trip in
# each direction, so every packet meets real kernel delay. Needs root and
# NET_ADMIN, so CI runs it in a privileged container.
#
#   scripts/screen-fps-lab.sh BIN_DIR [RTT_MS...]   # default: 0 150 300
#
# BIN_DIR holds atur-term, atur-term-server and screen_probe. Prints one JSON
# object per run and appends them to $GITHUB_STEP_SUMMARY.
set -euo pipefail

BIN=$(cd "${1:?usage: screen-fps-lab.sh BIN_DIR [RTT_MS...]}" && pwd)
shift || true
RTTS=("$@")
[[ ${#RTTS[@]} -gt 0 ]] || RTTS=(0 150 300)
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
export XDG_RUNTIME_DIR="$WORK/run"
export HOME="$WORK/home"
mkdir -m 700 "$XDG_RUNTIME_DIR" "$HOME"
export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman
SUMMARY=${GITHUB_STEP_SUMMARY:-/dev/null}
SECONDS_PER_RUN=${SECONDS_PER_RUN:-12}

cleanup() {
  status=$?
  tc qdisc del dev lo root 2>/dev/null || true
  if [[ $status -ne 0 ]]; then
    for log in sway server wayvnc; do
      echo "::group::$log log"
      tail -200 "$WORK/$log.log" 2>/dev/null || true
      echo "::endgroup::"
    done
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

sway --unsupported-gpu -c "$HERE/screen-fps-lab/sway.conf" >"$WORK/sway.log" 2>&1 &
wait_for "the sway socket" bash -c 'ls "$XDG_RUNTIME_DIR"/wayland-[0-9]'
export WAYLAND_DISPLAY=$(basename "$(ls "$XDG_RUNTIME_DIR"/wayland-[0-9] | head -1)")
wait_for "sway's IPC socket" bash -c 'ls "$XDG_RUNTIME_DIR"/sway-ipc.*.sock'
export SWAYSOCK=$(ls "$XDG_RUNTIME_DIR"/sway-ipc.*.sock | head -1)
wait_for "an output" bash -c 'swaymsg -t get_outputs | grep -q HEADLESS'

# The server starts wayvnc itself; this one keeps its debug log, where neatvnc says
# when it drops a frame because the link looks full.
REAL_WAYVNC=$(command -v wayvnc)
cat >"$WORK/wayvnc" <<WRAP
#!/bin/bash
args=()
for argument in "\$@"; do
  [[ \$argument == warning ]] && argument=debug
  args+=("\$argument")
done
exec "$REAL_WAYVNC" "\${args[@]}" 2>>"$WORK/wayvnc.log"
WRAP
chmod +x "$WORK/wayvnc"

mkdir -p "$WORK/srv"
ATUR_TERM_DIR="$WORK/srv" ATUR_TERM_SCREEN_GPU=0 ATUR_TERM_WAYVNC="$WORK/wayvnc" SHELL=/bin/sh \
  "$BIN/atur-term-server" >"$WORK/server.log" 2>&1 &
wait_for "the server socket" test -S "$WORK/srv/server.sock"

# A fake ssh that starts the bridge on this computer and leaves it running when ssh
# goes, as sshd does; the client then speaks UDP to it.
cat >"$WORK/fake-ssh" <<FAKE
#!/bin/sh
export ATUR_TERM_DIR="$WORK/srv" PATH="$BIN:\$PATH" SHELL=/bin/sh
unset ATUR_TERM_SERVER_BIN ATUR_TERM_PANE_ID
for last; do :; done
exec 3<&0
/bin/sh -c "exec \$last" <&3 3<&- &
wait \$!
FAKE
chmod +x "$WORK/fake-ssh"

probe() { "$BIN/screen_probe" --udp "$WORK/fake-ssh" "$@"; }

# Half the round trip each way.
delay() {
  if [[ $1 -eq 0 ]]; then
    tc qdisc del dev lo root 2>/dev/null || true
  else
    tc qdisc replace dev lo root netem delay "$(($1 / 2))ms" limit 100000
  fi
}

REPORT=""
note() { echo "$1"; REPORT+="$1"$'\n'; }

echo "== how long a change takes to show, still desktop"
for rtt in "${RTTS[@]}"; do
  delay "$rtt"
  note "{\"rtt_ms\":$rtt,\"run\":$(probe colors 8)}"
done

# Something that repaints every frame callback, at the size its client asks for.
weston-simple-shm >"$WORK/animation.log" 2>&1 &
wait_for "the animation window" bash -c 'swaymsg -t get_tree | grep -q weston'
sleep 1

echo "== pictures a second while a window animates"
for rtt in "${RTTS[@]}"; do
  delay "$rtt"
  dropped_before=$(grep -c "Exceeded bandwidth limit" "$WORK/wayvnc.log" || true)
  result=$(probe fps "$SECONDS_PER_RUN")
  dropped_after=$(grep -c "Exceeded bandwidth limit" "$WORK/wayvnc.log" || true)
  note "{\"rtt_ms\":$rtt,\"frames_wayvnc_withheld\":$((dropped_after - dropped_before)),\"run\":$result}"
done
delay 0

{
  echo "### Screen mirroring over UDP, delayed loopback, headless sway, wayvnc $(wayvnc --version 2>&1 | head -1)"
  echo
  echo '```json'
  printf '%s' "$REPORT"
  echo '```'
} >>"$SUMMARY"
echo "screen frame rate lab: done"
