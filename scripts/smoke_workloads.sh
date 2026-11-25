#!/usr/bin/env bash
# Production-image workload smoke for nautilusd + its nautilus client:
#   nginx (HTTP serving + host->pod reachability), redis (kv roundtrip),
#   postgres (initdb, env, SQL roundtrip), alpine (minimal rootfs),
#   traefik/whoami (tiny HTTP service).
#
# Usage:
#   NAUTILUS_REMOTE=<host> scripts/smoke_workloads.sh            # sync+build+run
#   NAUTILUS_REMOTE=<host> scripts/smoke_workloads.sh --no-sync  # reuse tree
set -euo pipefail

HOST="${NAUTILUS_REMOTE:?NAUTILUS_REMOTE=<host> required}"
REMOTE_DIR='~/nautilus'
SOCK=/run/nautilus-test.sock
LOG=/tmp/nautilus-wl-smoke.log
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
        --exclude target --exclude .git --exclude '*.sock' \
        ./ "$HOST:$REMOTE_DIR/"
fi

echo "=== build (release, remote) ==="
ssh $SSHOPTS "$HOST" "set -o pipefail; export PATH=\$HOME/.cargo/bin:\$PATH; cd $REMOTE_DIR && \
    cargo build --release --target $TARGET 2>&1 | tail -3"

BIN="$REMOTE_DIR/target/$TARGET/release/nautilusd"
CTL="$REMOTE_DIR/target/$TARGET/release/nautilus"

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

ct() {
    local quoted="" a
    for a in "$@"; do quoted+=" $(printf '%q' "$a")"; done
    ssh $SSHOPTS "$HOST" "${PFX}$CTL --socket $SOCK$quoted"
}

FAILURES=0
expect() { local d="$1" p="$2"; shift 2
    if "$@" 2>/dev/null | grep -q -- "$p"; then echo "WL-PASS: $d"; else echo "WL-FAIL: $d"; FAILURES=$((FAILURES+1)); fi }
absent() { local d="$1" p="$2"; shift 2
    if "$@" 2>/dev/null | grep -q -- "$p"; then echo "WL-FAIL: $d"; FAILURES=$((FAILURES+1)); else echo "WL-PASS: $d"; fi }
must() { if ! "$@" >/dev/null 2>&1; then echo "WL-HARDFAIL (see above)"; exit 1; fi }

# pod ip of a ctl container from its inspect document
pod_ip() { ct inspect "$1" | sed -n 's/.*"Ip": "\(.*\)".*/\1/p' | head -1; }

echo "=== nginx: HTTP serving across netns ==="
must ct pull docker.io/library/nginx:latest
NG=$(ct run -d --name ngx1 docker.io/library/nginx:latest)
sleep 3
NG_IP=$(pod_ip ngx1)
[ -n "$NG_IP" ] || { echo "WL-HARDFAIL: no pod ip for nginx"; exit 1; }
expect "nginx serves on pod ip ($NG_IP)" "Welcome to nginx" \
    ssh $SSHOPTS "$HOST" "curl -s --max-time 5 http://$NG_IP/"
expect "nginx logs show workers" "start worker processes" ct logs ngx1
must ct exec ngx1 nginx -t
must ct restart ngx1
sleep 2
expect "nginx alive after restart" "Welcome to nginx" \
    ssh $SSHOPTS "$HOST" "curl -s --max-time 5 http://$NG_IP/"

echo "=== redis: kv roundtrip ==="
must ct run -d --name rds1 docker.io/library/redis:7-alpine
sleep 2
expect "redis ping -> PONG" PONG ct exec rds1 redis-cli ping
expect "redis set/get" "^v1$" ct exec rds1 sh -c 'redis-cli set k1 v1 >/dev/null && redis-cli get k1'
expect "redis bgrewrite log line" "Ready to accept connections" ct logs rds1

echo "=== postgres: initdb + sql roundtrip ==="
pg_password=$(openssl rand -hex 24)
PG=$(ct run -d --name pg1 -e "POSTGRES_PASSWORD=$pg_password" -e POSTGRES_DB=appdb \
        docker.io/library/postgres:16-alpine)
unset pg_password
[ -n "$PG" ] || { echo "WL-HARDFAIL: postgres run failed"; exit 1; }
READY=0
for i in $(seq 1 30); do
    if ct logs pg1 2>/dev/null | grep -q "ready to accept connections"; then READY=1; break; fi
    sleep 2
done
[ "$READY" = "1" ] || { echo "WL-HARDFAIL: postgres not ready in 60s; last logs:"; ct logs pg1 | tail -20 || true; exit 1; }
echo "WL-PASS: postgres initdb + startup (${i}x2s)"
expect "postgres version string" PostgreSQL \
    ct exec pg1 psql -U postgres -d appdb -tAc "select version()"
expect "postgres create/insert/select roundtrip" "^42$" \
    ct exec pg1 sh -c 'psql -U postgres -d appdb -q -c "CREATE TABLE demo(id int); INSERT INTO demo VALUES (42);" && psql -U postgres -d appdb -tAc "SELECT id FROM demo"'
must ct rm -f pg1

echo "=== alpine: minimal rootfs ==="
AL=$(ct run --rm --name alp1 docker.io/library/alpine:3.19 sh -c 'uname -m && echo alpine-ok')
expect "alpine exec output" alpine-ok printf '%s' "$AL"
expect "alpine arch matches host" "$ARCH" printf '%s' "$AL"

echo "=== whoami: tiny HTTP service ==="
must ct run -d --name who1 docker.io/traefik/whoami:latest
sleep 2
WHO_IP=$(pod_ip who1)
expect "whoami serves on pod ip ($WHO_IP)" Hostname \
    ssh $SSHOPTS "$HOST" "curl -s --max-time 5 http://$WHO_IP/"

echo "=== images inventory ==="
for img in nginx redis postgres alpine whoami; do
    expect "image present: $img" "$img" ct images
done

echo "=== cleanup ==="
must ct rm -f ngx1
must ct rm -f rds1
must ct rm -f who1
absent "all workloads removed" ngx1 ct ps -a
absent "no leftover running" Running ct ps

if [ "$FAILURES" -eq 0 ]; then
    echo "ALL-WORKLOAD-PASS"
else
    echo "WORKLOAD FAILURES: $FAILURES"
fi
ssh $SSHOPTS "$HOST" "${PFX}systemctl stop nautilus-smoke 2>/dev/null; ${PFX}pkill -x nautilus 2>/dev/null || true"
