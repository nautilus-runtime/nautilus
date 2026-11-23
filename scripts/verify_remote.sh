#!/usr/bin/env bash
# P0 remote verification for nautilus:
#   sync source -> release build -> run daemon -> exercise runc AND wasi
#   sandboxes through crictl against the CRI socket -> assert results.
#
# Runs against any SSH-reachable Linux host (x86_64 or aarch64); the build
# target is selected from the remote architecture.
#
# Usage:
#   NAUTILUS_REMOTE=<host> scripts/verify_remote.sh             # full: sync + build + verify
#   NAUTILUS_REMOTE=<host> scripts/verify_remote.sh --no-sync   # reuse last synced tree
#   NAUTILUS_REMOTE=<host> scripts/verify_remote.sh --check     # sync + cargo check only
#   NAUTILUS_PROXY=<url>   scripts/verify_remote.sh             # egress proxy for the remote host
set -euo pipefail

HOST="${NAUTILUS_REMOTE:?NAUTILUS_REMOTE=<host> required}"
# Quoted tilde: must expand on the REMOTE side, not locally.
REMOTE_DIR='~/nautilus'
SOCK=/run/nautilus-test.sock
LOG=/tmp/nautilus-test.log
# Isolated state roots for the test daemon (mirrored by the config rewrite
# in the daemon-start section) — never the deployed service's paths.
TEST_STATE=/var/lib/nautilus-test
TEST_RUN=/var/run/nautilus-test

cd "$(dirname "$0")/.."

mode="${1:-full}"

# The public-internet link to the host drops idle TCP silently; keepalives are
# mandatory for every connection, and short-lived commands are preferred over
# one session spanning long builds.
SSHOPTS="-o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"

# Target triple follows the REMOTE arch. The repo ships .cargo/config.toml
# pinning x86_64-unknown-linux-gnu; the explicit --target below overrides it,
# so every host builds native glibc regardless of its architecture.
ARCH=$(ssh $SSHOPTS "$HOST" 'uname -m')
case "$ARCH" in
    x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
    aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
    *) echo "unsupported remote arch ($ARCH) on $HOST" >&2; exit 1 ;;
esac

# Non-root logins drive the privileged runtime paths (state dirs, netns
# mounts, systemd-run) through passwordless sudo; root hosts run unprefixed.
if [ "$(ssh $SSHOPTS "$HOST" 'id -u')" = "0" ]; then SUDO=""; else SUDO="sudo -n "; fi

# Absolute remote home (for config paths): $HOME cannot be used inside the
# daemon-start sed, which is single-quoted on the remote side.
REMOTE_HOME="$(ssh $SSHOPTS "$HOST" 'cd ~ && pwd')"

# Egress proxy for the remote host (hosts behind restricted networks need one
# to reach registries/GitHub). NAUTILUS_PROXY overrides per run (set it to the
# empty string to force direct egress).
PROXY="${NAUTILUS_PROXY:-}"
PENV=""
ENVARGS=""
[ -n "$PROXY" ] && PENV="export https_proxy=$PROXY http_proxy=$PROXY;"
[ -n "$PROXY" ] && ENVARGS="-p Environment=HTTP_PROXY=$PROXY -p Environment=HTTPS_PROXY=$PROXY -p Environment=NO_PROXY=localhost,127.0.0.1"
echo "=== host=$HOST arch=$ARCH target=$TARGET proxy=${PROXY:-direct} sudo=${SUDO:+yes} ==="

echo "=== sync source ==="
if [ "$mode" != "--no-sync" ]; then
    rsync -az --delete \
        -e "ssh $SSHOPTS" \
        --exclude target --exclude target-root --exclude .git --exclude '*.sock' \
        ./ "$HOST:$REMOTE_DIR/"
fi

if [ "$mode" = "--check" ] || [ "$mode" = "--check-all" ]; then
    targets=""
    [ "$mode" = "--check-all" ] && targets="--all-targets"
    # pipefail so tail cannot mask cargo's exit status
    ssh $SSHOPTS "$HOST" "set -o pipefail; $PENV \
        cd $REMOTE_DIR && ~/.cargo/bin/cargo check --target $TARGET $targets 2>&1 | tail -60"
    exit $?
fi

echo "=== build (release, remote) ==="
ssh $SSHOPTS "$HOST" "set -o pipefail; $PENV export PATH=\$HOME/.cargo/bin:\$PATH; \
    cd $REMOTE_DIR && cargo build --release --target $TARGET 2>&1 | tail -5"
BIN="$REMOTE_DIR/target/$TARGET/release/nautilusd"

echo "=== build demo policy wasm (P1b fixture) ==="
ssh $SSHOPTS "$HOST" "set -o pipefail; $PENV export PATH=\$HOME/.cargo/bin:\$PATH; \
    cd $REMOTE_DIR/examples/policy-deny-label && \
    rustup target list --installed | grep -q wasm32-unknown-unknown || rustup target add wasm32-unknown-unknown; \
    cargo build --release --target wasm32-unknown-unknown 2>&1 | tail -2; \
    test -f target/wasm32-unknown-unknown/release/policy_deny_label.wasm"

echo "=== fixtures ==="
ssh $SSHOPTS "$HOST" 'cat > /tmp/hello.wat <<"WAT"
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 8) "hello-nautilus-wasm\n")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 8))
    (i32.store (i32.const 4) (i32.const 20))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 24)))))
WAT'

echo "=== start daemon ==="
# Run as a systemd transient unit: the daemon's lifecycle is fully decoupled
# from the ssh session. (Every shell-spawn variant eventually hung ssh in
# do_wait() — bash kept the backgrounded daemon as a direct child.)
#
# The Wasm policy engine (P1b) is enabled for the whole run via a generated
# config: normal traffic must flow (allow path) and a deny label must block.
# The test daemon is fully isolated from any deployed nautilusd: its own
# socket, unit and *-test state/netns/runc paths (rewritten below), so the
# suite can run alongside a production service on the same host.
ssh $SSHOPTS "$HOST" "${SUDO}systemctl stop nautilus-test 2>/dev/null; \
    ${SUDO}sh -c 'for m in /var/run/nautilus-test/netns/*; do umount \"\$m\" 2>/dev/null; done'; \
    ${SUDO}rm -rf /var/lib/nautilus-test /run/nautilus-test /var/run/nautilus-test; \
    ${SUDO}rm -f $SOCK /tmp/nautilus-test.log /tmp/nau-test.toml; \
    $BIN --dump-config /tmp/nau-test.toml >/dev/null; \
    ${SUDO}sed -i 's|/var/lib/nautilus|/var/lib/nautilus-test|g; s|/var/run/nautilus/netns|/var/run/nautilus-test/netns|g; s|/run/nautilus/runc|/run/nautilus-test/runc|g' /tmp/nau-test.toml; \
    sed -i '/^\[policy\]/,$ s/^enabled = false/enabled = true/' /tmp/nau-test.toml; \
    sed -i '/^\[policy\]/,$ s|^path = \"\"|path = \"$REMOTE_HOME/nautilus/examples/policy-deny-label/target/wasm32-unknown-unknown/release/policy_deny_label.wasm\"|' /tmp/nau-test.toml; \
    ${SUDO}systemd-run --unit=nautilus-test --collect \
        -p StandardOutput=append:/tmp/nautilus-test.log \
        -p StandardError=append:/tmp/nautilus-test.log \
        $ENVARGS \
        $BIN --config /tmp/nau-test.toml --socket $SOCK --log-level info > /dev/null; \
    sleep 2; \
    test -S $SOCK && echo DAEMON-UP || (${SUDO}tail -20 /tmp/nautilus-test.log; exit 1)"

run_cri() {
    # Re-quote every argument so constructs like `-c "echo exec-ok"` survive
    # the ssh hop (plain $* would strip the inner quotes and change semantics).
    local quoted=""
    local a
    for a in "$@"; do
        quoted+=" $(printf '%q' "$a")"
    done
    ssh $SSHOPTS "$HOST" "${SUDO}crictl -r unix://$SOCK --timeout 60s$quoted"
}

echo "=== runc path: busybox echo ==="
ssh $SSHOPTS "$HOST" 'cat > /tmp/pod-runc.json <<"EOF"
{
  "metadata": {"name": "p-runc", "namespace": "default", "uid": "uid-runc-1"},
  "linux": {}
}
EOF
cat > /tmp/ctr-runc.json <<"EOF"
{
  "metadata": {"name": "c-echo"},
  "image": {"image": "docker.io/library/busybox:latest"},
  "command": ["/bin/sh", "-c", "echo hello-nautilus-runc"],
  "linux": {}
}
EOF'
PULL_OUT=$(run_cri pull docker.io/library/busybox:latest) || {
    echo "pull failed; diagnostics:" >&2
    ssh $SSHOPTS "$HOST" "tail -30 /tmp/nautilus-test.log; docker images | head -5" >&2
    exit 1
}
echo "pull: $PULL_OUT"

POD_RUNC=$(run_cri runp /tmp/pod-runc.json)
CTR_RUNC=$(run_cri create "$POD_RUNC" /tmp/ctr-runc.json /tmp/pod-runc.json)
run_cri start "$CTR_RUNC" > /dev/null
sleep 1

STATE=$(run_cri ps -a --id "$CTR_RUNC" -o table 2>/dev/null | tail -1 || true)
echo "runc ctr row: $STATE"
RAW=$(ssh $SSHOPTS "$HOST" "${SUDO}cat ${TEST_STATE}/containers/$CTR_RUNC-out/out.log 2>/dev/null || echo NO-LOG")
# CRI log format: `<RFC3339Z> <stream> F <payload>` — strip the 3-field prefix.
OUT=$(printf '%s' "$RAW" | sed -E 's/^[^ ]+ [^ ]+ F //')
echo "runc output: $OUT"
[ "$OUT" = "hello-nautilus-runc" ] && echo "RUNC-PASS" || { echo "RUNC-FAIL"; exit 1; }

echo "=== pod network: netns + CNI IP ==="
NET_IP=$(run_cri inspectp "$POD_RUNC" | python3 -c "import json,sys;d=json.load(sys.stdin);print(d['status'].get('network',{}).get('ip',''))" 2>/dev/null)
[ -n "$NET_IP" ] && echo "NET-PASS ip=$NET_IP" || { echo "NET-FAIL (no ip)"; exit 1; }
ssh $SSHOPTS "$HOST" "${SUDO}python3 - <<PY
import json
spec=json.load(open('${TEST_STATE}/containers/$CTR_RUNC/config.json'))
ns=[n for n in spec['linux']['namespaces'] if n['type']=='network']
assert ns and ns[0].get('path','').startswith('"$TEST_RUN"/netns'), ns
print('NS-JOIN-OK', ns[0]['path'])
PY" || { echo "NS-JOIN-FAIL"; exit 1; }
run_cri stop "$CTR_RUNC" > /dev/null || true
run_cri rm "$CTR_RUNC" > /dev/null
run_cri stopp "$POD_RUNC" > /dev/null
run_cri rmp "$POD_RUNC" > /dev/null

echo "=== resource limits: CRI → OCI spec ==="
ssh $SSHOPTS "$HOST" 'cat > /tmp/ctr-res.json <<"EOF"
{
  "metadata": {"name": "c-res"},
  "image": {"image": "docker.io/library/busybox:latest"},
  "command": ["/bin/sh", "-c", "sleep 30"],
  "linux": {
    "resources": {
      "memory_limit_in_bytes": 33554432,
      "cpu_shares": 512,
      "cpuset_cpus": "0"
    }
  }
}
EOF'
POD_RES=$(run_cri runp /tmp/pod-runc.json)
CTR_RES=$(run_cri create "$POD_RES" /tmp/ctr-res.json /tmp/pod-runc.json)
ssh $SSHOPTS "$HOST" "${SUDO}python3 - <<PY
import json,sys
spec=json.load(open('${TEST_STATE}/containers/$CTR_RES/config.json'))
res=spec['linux']['resources']
mem=res['memory']['limit']; shares=res['cpu']['shares']
assert mem==33554432 and shares==512, res
print('RES-OK', mem, shares)
PY" || { echo "RESOURCES-FAIL"; exit 1; }
run_cri start "$CTR_RES" > /dev/null
sleep 1
run_cri stats --id "$CTR_RES" -o json 2>/dev/null | python3 -c "
import json,sys
d=json.load(sys.stdin)
stats=d.get('stats',[])
assert stats, 'no stats returned'
m=stats[0]['memory']['workingSetBytes']['value']
c=stats[0]['cpu']['usageCoreNanoSeconds']['value']
print('STATS-PASS mem=%s cpu=%s' % (m, c))" || { echo "STATS-FAIL"; exit 1; }
run_cri rm -f "$CTR_RES" > /dev/null 2>&1 || true
run_cri stopp "$POD_RES" > /dev/null 2>&1 || true
run_cri rmp "$POD_RES" > /dev/null 2>&1 || true

echo "=== wasi path: hello.wat via wasmtime ==="
ssh $SSHOPTS "$HOST" 'cat > /tmp/pod-wasi.json <<"EOF"
{
  "metadata": {"name": "p-wasi", "namespace": "default", "uid": "uid-wasi-1"},
  "linux": {}
}
EOF
cat > /tmp/ctr-wasi.json <<"EOF"
{
  "metadata": {"name": "c-wasm"},
  "image": {"image": "file:///tmp/hello.wat"},
  "linux": {}
}
EOF'
POD_WASI=$(run_cri runp --runtime wasi /tmp/pod-wasi.json)
CTR_WASI=$(run_cri create "$POD_WASI" /tmp/ctr-wasi.json /tmp/pod-wasi.json)
run_cri start "$CTR_WASI" > /dev/null
sleep 1
WASI_STATE=$(run_cri ps -a --id "$CTR_WASI" 2>/dev/null | tail -1 || true)
echo "wasi ctr row: $WASI_STATE"
echo "$WASI_STATE" | grep -q "Exited" && echo "WASI-PASS" || { echo "WASI-FAIL"; exit 1; }
run_cri rm "$CTR_WASI" > /dev/null
run_cri stopp "$POD_WASI" > /dev/null
run_cri rmp "$POD_WASI" > /dev/null

echo "=== unknown handler must be rejected ==="
if run_cri runp --runtime no-such-handler /tmp/pod-runc.json 2>/dev/null; then
    echo "REJECT-FAIL"; exit 1
else
    echo "REJECT-PASS"
fi

echo "=== same-pod containers share one netns ==="
# Both container specs must exist BEFORE this section: a fresh host has no
# leftover /tmp/ctr-exec.json from earlier runs (latent ali-only masking).
ssh $SSHOPTS "$HOST" 'cat > /tmp/ctr-exec.json <<"EOF"
{
  "metadata": {"name": "c-exec"},
  "image": {"image": "docker.io/library/busybox:latest"},
  "command": ["/bin/sh", "-c", "i=0; while [ $i -lt 300 ]; do sleep 1; i=$((i+1)); done"],
  "linux": {}
}
EOF
cat > /tmp/ctr-exec2.json <<"EOF"
{
  "metadata": {"name": "c-exec2"},
  "image": {"image": "docker.io/library/busybox:latest"},
  "command": ["/bin/sh", "-c", "i=0; while [ $i -lt 120 ]; do sleep 1; i=$((i+1)); done"],
  "linux": {}
}
EOF'
POD_SH=$(run_cri runp /tmp/pod-runc.json)
CTR_A=$(run_cri create "$POD_SH" /tmp/ctr-exec.json /tmp/pod-runc.json)
CTR_B=$(run_cri create "$POD_SH" /tmp/ctr-exec2.json /tmp/pod-runc.json)
run_cri start "$CTR_A" > /dev/null
run_cri start "$CTR_B" > /dev/null
sleep 1
ssh $SSHOPTS "$HOST" "${SUDO}python3 - <<PY
import json
a=json.load(open('${TEST_STATE}/containers/$CTR_A/config.json'))
b=json.load(open('${TEST_STATE}/containers/$CTR_B/config.json'))
na=[n for n in a['linux']['namespaces'] if n['type']=='network'][0]
nb=[n for n in b['linux']['namespaces'] if n['type']=='network'][0]
assert na['path']==nb['path'], (na, nb)
assert na['path'].startswith('"$TEST_RUN"/netns'), na
print('SHARED-NETNS-OK', na['path'])
PY" || { echo "SHARED-NETNS-FAIL"; exit 1; }
ST_B=$(run_cri ps -a --id "$CTR_B" 2>/dev/null | tail -1 || true)
case "$ST_B" in *Running*) echo "CO-RUN-PASS" ;; *) echo "CO-RUN-FAIL ($ST_B)"; exit 1 ;; esac
if run_cri exec --sync "$CTR_B" /bin/sh -c "grep -q eth0 /proc/net/route && echo net-stack-ok" 2>/dev/null | grep -q net-stack-ok; then
    echo "NETSTACK-PASS"
else
    echo "NETSTACK-FAIL"; exit 1
fi
run_cri rm -f "$CTR_A" > /dev/null 2>&1 || true
run_cri rm -f "$CTR_B" > /dev/null 2>&1 || true
run_cri stopp "$POD_SH" > /dev/null 2>&1 || true
run_cri rmp "$POD_SH" > /dev/null 2>&1 || true

echo "=== wasm policy engine: deny-by-label ==="
# Policy is live for this whole run (see daemon start). A pod whose labels
# carry nautilus.policy/deny must be refused with the policy's own reason.
ssh $SSHOPTS "$HOST" 'cat > /tmp/pod-deny.json <<"EOF"
{
  "metadata": {"name": "p-deny", "namespace": "default", "uid": "uid-deny-1"},
  "labels": {"nautilus.policy/deny": "true"},
  "linux": {}
}
EOF'
if POLICY_ERR=$(run_cri runp /tmp/pod-deny.json 2>&1); then
    echo "POLICY-FAIL (deny-label pod was allowed)"; exit 1
else
    echo "$POLICY_ERR" | grep -q "denied by policy" || { echo "POLICY-FAIL wrong error: $POLICY_ERR"; exit 1; }
    echo "POLICY-DENY-PASS"
fi
# exec_sync on a long-running container.
ssh $SSHOPTS "$HOST" 'cat > /tmp/ctr-exec.json <<"EOF"
{
  "metadata": {"name": "c-exec"},
  "image": {"image": "docker.io/library/busybox:latest"},
  "command": ["/bin/sh", "-c", "i=0; while [ $i -lt 300 ]; do sleep 1; i=$((i+1)); done"],
  "linux": {}
}
EOF'
POD_EX=$(run_cri runp /tmp/pod-runc.json)
CTR_EX=$(run_cri create "$POD_EX" /tmp/ctr-exec.json /tmp/pod-runc.json)
run_cri start "$CTR_EX" > /dev/null
sleep 1
# First exec can hit a cold proxy window on a fresh state db; retry once.
# NOTE: --sync routes crictl to the ExecSync RPC (streaming exec is not
# implemented). The if-form avoids set -e killing the script on a failed
# `[ ] && break`.
# crictl appends blank lines after the captured stdout, so grep for the
# marker instead of taking the last line.
EXEC_OUT=""
for attempt in 1 2; do
    if run_cri exec --sync "$CTR_EX" /bin/sh -c "echo exec-ok" 2>/dev/null | grep -q "exec-ok"; then
        EXEC_OUT="exec-ok"
        break
    fi
    sleep 2
done
[ "$EXEC_OUT" = "exec-ok" ] && echo "EXEC-SYNC-PASS" || { echo "EXEC-SYNC-FAIL out=$EXEC_OUT"; exit 1; }
# Older crictl on the host names this RPC "info"; both take -o json.
STATUS_OK=$(run_cri info -o json 2>/dev/null | python3 -c "import json,sys;d=json.load(sys.stdin);cs=d['status']['conditions'];print('ok' if any(c['type']=='RuntimeReady' and c['status'] for c in cs) and any(c['type']=='NetworkReady' and c['status'] for c in cs) else 'bad')" 2>/dev/null)
if [ "$STATUS_OK" != "ok" ]; then
    STATUS_OK=$(run_cri status -o json 2>/dev/null | python3 -c "import json,sys;d=json.load(sys.stdin);cs=d['status']['conditions'];print('ok' if any(c['type']=='RuntimeReady' and c['status'] for c in cs) and any(c['type']=='NetworkReady' and c['status'] for c in cs) else 'bad')" 2>/dev/null)
fi
[ "$STATUS_OK" = "ok" ] && echo "STATUS-PASS" || { echo "STATUS-FAIL"; exit 1; }
run_cri rm -f "$CTR_EX" > /dev/null 2>&1 || true
run_cri stopp "$POD_EX" > /dev/null 2>&1 || true
run_cri rmp "$POD_EX" > /dev/null 2>&1 || true

echo "ALL-P0-PASS"

ssh $SSHOPTS "$HOST" "${SUDO}systemctl stop nautilus-test 2>/dev/null"
