#!/usr/bin/env bash
# End-to-end smoke test for the nautilus client against nautilusd.
#   build ctl + daemon -> start daemon -> exercise the whole ctl surface -> assert.
#
# Usage:
#   NAUTILUS_REMOTE=<host> scripts/smoke_ctl.sh            # sync + build + smoke
#   NAUTILUS_REMOTE=<host> scripts/smoke_ctl.sh --no-sync  # reuse last synced tree
#
# No hostnames/secrets live here: everything comes from the environment.
set -euo pipefail

HOST="${NAUTILUS_REMOTE:?NAUTILUS_REMOTE=<host> required}"
REMOTE_DIR='~/nautilus'
SOCK=/run/nautilus-test.sock
LOG=/tmp/nautilus-smoke.log
# Build target follows the remote architecture (like verify_remote.sh).
SSHOPTS="-o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
ARCH=$(ssh $SSHOPTS "$HOST" 'uname -m')
case "$ARCH" in
    x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
    aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
    *) echo "unsupported remote arch ($ARCH) on $HOST" >&2; exit 1 ;;
esac

cd "$(dirname "$0")/.."
mode="${1:-full}"

if [ "$mode" != "--no-sync" ]; then
    rsync -az --delete -e "ssh $SSHOPTS" \
        --exclude target --exclude target-root --exclude .git --exclude '*.sock' \
        ./ "$HOST:$REMOTE_DIR/"
fi

echo "=== build (release, remote) ==="
ssh $SSHOPTS "$HOST" "set -o pipefail; export PATH=\$HOME/.cargo/bin:\$PATH; cd $REMOTE_DIR && \
    cargo build --release --target $TARGET 2>&1 | tail -3"

BIN="$REMOTE_DIR/target/$TARGET/release/nautilusd"
CTL="$REMOTE_DIR/target/$TARGET/release/nautilus"

# Non-root logins need sudo for the root-owned socket and state dirs.
if [ "$(ssh $SSHOPTS "$HOST" 'id -u')" = "0" ]; then PFX=""; else PFX="sudo "; fi

echo "=== start daemon ==="
ssh $SSHOPTS "$HOST" "${PFX}pkill -x nautilusd 2>/dev/null || true; ${PFX}pkill -x nautilus 2>/dev/null || true; \
    ${PFX}systemctl stop nautilus-smoke 2>/dev/null || true; \
    ${PFX}sh -c 'for m in /var/run/nautilus/netns/*; do umount \"\$m\" 2>/dev/null; done'; \
    ${PFX}rm -rf /var/lib/nautilus/containers /var/lib/nautilus/state.db /run/nautilus/runc /var/run/nautilus/netns; \
    ${PFX}rm -f $SOCK /tmp/nau-ctl.toml $LOG; \
    $BIN --dump-config /tmp/nau-ctl.toml >/dev/null; \
    ${PFX}systemd-run --unit=nautilus-smoke --collect \
        -p StandardOutput=append:$LOG \
        -p StandardError=append:$LOG \
        $BIN --config /tmp/nau-ctl.toml --socket $SOCK --log-level info >/dev/null; \
    sleep 2; test -S $SOCK && echo DAEMON-UP || (${PFX}tail -20 $LOG; exit 1)"

# ct <args...> — run the nautilus client on the remote against the test socket.
# Every argument is shell-quoted across the ssh hop (printf %q) so compound
# commands like `sh -c 'while …; do …; done'` arrive intact.
ct() {
    local quoted="" a
    for a in "$@"; do quoted+=" $(printf '%q' "$a")"; done
    ssh $SSHOPTS "$HOST" "${PFX}$CTL --socket $SOCK$quoted"
}

FAILURES=0
expect() { # expect <desc> <pattern> <cmd...>: stdout must contain pattern
    local desc="$1" pat="$2"; shift 2
    if "$@" 2>/dev/null | grep -q -- "$pat"; then
        echo "CTL-PASS: $desc"
    else
        echo "CTL-FAIL: $desc"; FAILURES=$((FAILURES+1))
    fi
}
refute() { # refute <desc> <cmd...>: command must fail / produce nothing usable
    local desc="$1"; shift
    if "$@" >/dev/null 2>&1; then
        echo "CTL-FAIL: $desc"; FAILURES=$((FAILURES+1))
    else
        echo "CTL-PASS: $desc"
    fi
}
absent() { # absent <desc> <pattern> <cmd...>: stdout must NOT contain pattern
    local desc="$1" pat="$2"; shift 2
    if "$@" 2>/dev/null | grep -q -- "$pat"; then
        echo "CTL-FAIL: $desc"; FAILURES=$((FAILURES+1))
    else
        echo "CTL-PASS: $desc"
    fi
}
must() { # must <cmd...>: hard-fail unless exit 0
    if ! "$@" >/dev/null 2>&1; then echo "CTL-HARDFAIL (see above)"; exit 1; fi
}

echo "=== version/info ==="
expect "version rpc"         "Runtime name" ct version
expect "info conditions"     "RuntimeReady" ct info

echo "=== pull/images ==="
must ct pull docker.io/library/busybox:latest
expect "images lists busybox" busybox ct images

echo "=== run detached + surface ==="
ID=$(ct run --name web1 -e GREETING=morning --memory 64m busybox \
        sh -c 'while true; do echo tick $GREETING; sleep 1; done')
[ -n "$ID" ] || { echo "CTL-HARDFAIL: run returned empty id"; exit 1; }
sleep 2
expect "ps shows running"       web1          ct ps
expect "env reaches container"  "tick morning" ct logs web1
expect "exec echo"              exec-via-ctl   ct exec web1 echo exec-via-ctl
expect "inspect running json"   '"State": "Running"' ct inspect web1
expect "stats row"              web1          ct stats
must ct stop web1
must ct start web1
must ct restart web1
must ct rm -f web1
absent "gone after rm" web1 ct inspect web1

echo "=== entrypoint override (docker merge semantics) ==="
EP=$(ct run --rm --name ep1 --entrypoint /bin/echo busybox ep-works)
expect "entrypoint+args merged" ep-works printf '%s' "$EP"

echo "=== --rm foreground semantics ==="
FG=$(ct run --rm --name fg1 busybox echo foreground-done)
expect "--rm waited for output" foreground-done printf '%s' "$FG"
refute "--rm cleaned up" bash -c "ct ps -a | grep -q fg1"

echo "=== wasm module through ctl ==="
ssh $SSHOPTS "$HOST" 'cat > /tmp/hello.wat <<"WAT"
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 8) "hello-ctl-wasm\n")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 8))
    (i32.store (i32.const 4) (i32.const 20))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 24)))))
WAT'
must ct run --rm --runtime wasi --name wsm1 file:///tmp/hello.wat
absent "wasm sandbox cleaned up" wsm1 ct ps -a

echo "=== prune ==="
must ct run --name pr1 busybox true
sleep 2   # allow the exit reaper to mark it Exited
must ct prune
absent "prune removed exited ctr" pr1 ct ps -a

echo "=== error surfaces ==="
refute "missing object errors cleanly" ct logs no-such-thing
must ct run --name dup busybox sleep 300
refute "duplicate name rejected" ct run --name dup busybox true
must ct rm -f dup

echo "=== publish -p ==="
PUB=$($CTL run --name pub-ng -p 18080:80 docker.io/library/nginx:latest)
[ -n "$PUB" ] || { echo "CTL FAILURES: hardfail publish run"; exit 1; }
sleep 3
expect "published hostport serves" "Welcome to nginx" \
    ssh $SSHOPTS "$HOST" "curl -s --max-time 4 http://127.0.0.1:18080/"
$CTL rm -f pub-ng >/dev/null 2>&1 || true
sleep 1
absent "port freed after rm" ":18080" ssh $SSHOPTS "$HOST" "${PFX}ss -tln"

if [ "$FAILURES" -eq 0 ]; then
    echo "ALL-CTL-PASS"
else
    echo "CTL FAILURES: $FAILURES"
    exit 1
fi

ssh $SSHOPTS "$HOST" "${PFX}systemctl stop nautilus-smoke 2>/dev/null; ${PFX}pkill -x nautilusd 2>/dev/null || true; ${PFX}pkill -x nautilus 2>/dev/null || true"
