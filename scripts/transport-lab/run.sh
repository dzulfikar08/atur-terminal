#!/bin/bash
# Measures keystroke echo, screen staleness and outage recovery over ssh and
# over UDP on emulated bad links, without root and without touching the real
# network.
#
# Everything runs in a private user, mount and network namespace whose
# loopback interface is shaped with netem, so ssh, sshd and the UDP transport
# all meet real kernel loss, delay and jitter. Nothing leaves this computer.
#
#   scripts/transport-lab/run.sh [--debug-dir DIR] [--keys N] [--stale SECONDS]
#                                [--transports "ssh udp"] [SCENARIO...]
#
# Scenarios (name:one-way-delay:jitter:loss): rtt50, rtt300, rtt800. The
# binaries come from DIR (default target/debug), which must hold atur-term,
# atur-term-server and examples/transport_lab. Results are appended to
# $LAB/results.jsonl as one JSON object per run.
set -euo pipefail

if [ -z "${ATUR_LAB_INSIDE:-}" ]; then
    here=$(cd "$(dirname "$0")" && pwd)
    export ATUR_LAB_INSIDE=1 ATUR_LAB_SCRIPT="$here/run.sh"
    exec unshare --user --map-root-user --map-auto --mount --net --pid --fork \
        --mount-proc -- "$ATUR_LAB_SCRIPT" "$@"
fi

BIN=$(cd "${BIN:-target/debug}" && pwd)
LAB=${LAB:-/tmp/atur-lab}
KEYS=40
STALE=20
OUTAGE=10
TRANSPORTS="ssh udp"
SCENARIOS=()
while [ $# -gt 0 ]; do
    case $1 in
        --debug-dir) BIN=$(cd "$2" && pwd); shift 2 ;;
        --keys) KEYS=$2; shift 2 ;;
        --stale) STALE=$2; shift 2 ;;
        --outage) OUTAGE=$2; shift 2 ;;
        --transports) TRANSPORTS=$2; shift 2 ;;
        *) SCENARIOS+=("$1"); shift ;;
    esac
done
[ ${#SCENARIOS[@]} -gt 0 ] || SCENARIOS=(rtt50 rtt300 rtt800)

rm -rf "$LAB"
mkdir -p "$LAB"/{ssh,home,state,empty,run}
chmod 700 "$LAB/ssh"

# sshd's privilege separation needs a user and an empty directory it owns.
sed 's/^root:[^:]*:/root:*:/' /etc/passwd > "$LAB/passwd"
grep -q '^sshd:' "$LAB/passwd" || echo 'sshd:x:74:74:sshd:/var/empty:/usr/bin/nologin' >> "$LAB/passwd"
mount --bind "$LAB/passwd" /etc/passwd
# An account with an empty password field is not "locked", so sshd lets keys in.
printf 'root::19000:0:99999:7:::\nsshd:!:19000:0:99999:7:::\n' > "$LAB/shadow"
chmod 600 "$LAB/shadow"
mount --bind "$LAB/shadow" /etc/shadow
for empty in /usr/share/empty.sshd /var/empty; do
    [ -d "$empty" ] && mount --bind "$LAB/empty" "$empty"
done

ip link set lo up
ip link set dev lo mtu 1500
ip link set dev lo gso_max_size 1514 gso_max_segs 1 2>/dev/null || true
ethtool -K lo tso off gso off gro off 2>/dev/null || true

ssh-keygen -q -t ed25519 -N '' -f "$LAB/ssh/host"
ssh-keygen -q -t ed25519 -N '' -f "$LAB/ssh/client"
cp "$LAB/ssh/client.pub" "$LAB/ssh/authorized_keys"

cat > "$LAB/ssh/sshd_config" <<CONF
Port 2222
ListenAddress 127.0.0.1
HostKey $LAB/ssh/host
PidFile $LAB/ssh/sshd.pid
AuthorizedKeysFile $LAB/ssh/authorized_keys
PermitRootLogin yes
UsePAM no
PasswordAuthentication no
KbdInteractiveAuthentication no
StrictModes no
LogLevel ${SSHD_LOG:-ERROR}
ForceCommand $LAB/forcecommand.sh
CONF

cat > "$LAB/forcecommand.sh" <<CONF
#!/bin/sh
export HOME=$LAB/home SHELL=/bin/sh PATH=$BIN:/usr/bin:/bin ATUR_TERM_DIR=$LAB/state
unset ATUR_TERM_SERVER_BIN ATUR_TERM_PANE_ID XDG_STATE_HOME
exec /bin/sh -c "\$SSH_ORIGINAL_COMMAND"
CONF
chmod +x "$LAB/forcecommand.sh"
printf 'default_shell = "/bin/sh"\nshell_integration = false\n' > "$LAB/state/server.toml"

cat > "$LAB/ssh_config" <<CONF
Host lab
  HostName 127.0.0.1
  Port 2222
  User root
  IdentityFile $LAB/ssh/client
  IdentitiesOnly yes
  UserKnownHostsFile /dev/null
  StrictHostKeyChecking no
  LogLevel ERROR
CONF
printf '#!/bin/sh\nexec /usr/bin/ssh -F %s "$@"\n' "$LAB/ssh_config" > "$LAB/sshw"
chmod +x "$LAB/sshw"

/usr/bin/sshd -f "$LAB/ssh/sshd_config" -E "$LAB/sshd.log"
trap 'kill "$(cat "$LAB/ssh/sshd.pid" 2>/dev/null)" 2>/dev/null || true' EXIT

netem() { tc qdisc replace dev lo root netem "$@"; }
netem delay 0ms

stop_server() {
    netem delay 0ms
    ATUR_TERM_DIR=$LAB/state "$BIN/atur-term" server stop --force >/dev/null 2>&1 || true
    pkill -f "atur-term stdio" 2>/dev/null || true
    sleep 1
}

for scenario in "${SCENARIOS[@]}"; do
    case $scenario in
        rtt50)  shape="delay 25ms" ;;
        rtt300) shape="delay 150ms 30ms loss 5%" ;;
        rtt800) shape="delay 400ms 200ms loss 10%" ;;
        *) echo "unknown scenario $scenario" >&2; exit 2 ;;
    esac
    for transport in $TRANSPORTS; do
        stop_server
        netem $shape
        flag=""
        [ "$transport" = udp ] && flag="--udp"
        rm -rf "$LAB/run"; mkdir -p "$LAB/run"
        result=$(ATUR_TERM_SSH="$LAB/sshw" ATUR_TERM_DIR="$LAB/client-state" \
            XDG_RUNTIME_DIR="$LAB/run" \
            "$BIN/examples/transport_lab" lab $flag --keys "$KEYS" --stale "$STALE" --outage "$OUTAGE" \
            --down "tc qdisc change dev lo root netem loss 100%" \
            --up "tc qdisc change dev lo root netem $shape" 2>"$LAB/last.err" | tail -1) || result='{"error":"failed"}'
        [ -s "$LAB/last.err" ] && cat "$LAB/last.err" >&2
        echo "{\"scenario\":\"$scenario\",\"shape\":\"$shape\",\"run\":$result}" | tee -a "$LAB/results.jsonl"
    done
done
stop_server
