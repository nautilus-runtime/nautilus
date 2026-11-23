#!/usr/bin/env bash
# Measure nautilus daemon memory footprint (P2 "edge-light" budget).
#
# Reports RSS and PSS (proportional set size — fairer for shared libs) of a
# running daemon, sampled at idle. Run after verify_remote.sh so a live
# daemon exists on the remote host.
#
# Usage:
#   NAUTILUS_REMOTE=<host> scripts/measure_memory.sh
set -euo pipefail

HOST="${NAUTILUS_REMOTE:?NAUTILUS_REMOTE=<host> required}"
SSHOPTS="-o BatchMode=yes -o ConnectTimeout=10"

echo "=== daemon memory footprint (idle) ==="
ssh $SSHOPTS "$HOST" 'bash -s' <<'REMOTE'
PID=$(pgrep -x nautilus | head -1)
if [ -z "$PID" ]; then
    echo "NO-DAEMON"; exit 1
fi
RSS=$(awk '/VmRSS/{print $2}' /proc/$PID/status)
HWM=$(awk '/VmHWM/{print $2}' /proc/$PID/status)
PSS=$(awk '/^Pss:/{print $2}' /proc/$PID/smaps_rollup)
THREADS=$(awk '/Threads/{print $2}' /proc/$PID/status)
UP=$(ps -o etimes= -p $PID | tr -d ' ')
cat <<EOF
pid=$PID threads=$THREADS uptime=${UP}s
RSS = $((RSS/1024)) MiB (${RSS} kB)
PSS = $((PSS/1024)) MiB (${PSS} kB)
peak RSS (VmHWM) = $((HWM/1024)) MiB
EOF

# Top shared contributions (what a multi-daemon edge node would amortize).
awk '/^[0-9]/{if ($2 ~ /\.so|nautilus/) print $2, $12}' /proc/$PID/smaps 2>/dev/null \
    | sort -k1 -n -r | head -5 || true
REMOTE
