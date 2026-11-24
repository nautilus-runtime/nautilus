#!/usr/bin/env bash
# Build the Pod workload image on Linux for its native CPU architecture.
set -euo pipefail
here=$(cd -- "$(dirname -- "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
case "$(uname -m)" in
    x86_64) arch=amd64; checksum=50dcc0b257d724fe07f3852c2dda52340223f038227c7572644eec870741223e ;;
    aarch64) arch=arm64; checksum=07b52aee923cc432c38fe4fb6809a08fc6471994efed650af3635350588a20be ;;
    *) echo "Unsupported architecture" >&2; exit 1 ;;
esac
if [[ -z ${GUEST_AGENT_BIN:-} ]]; then
    export GUEST_AGENT_BIN="$tmp/nautilus-agent"
    (cd "$here/agent" && CGO_ENABLED=0 GOOS=linux GOARCH="$arch" go build -mod=readonly -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o "$GUEST_AGENT_BIN" .)
fi
if [[ -z ${GUEST_RUNC_BIN:-} ]]; then
    export GUEST_RUNC_BIN="$tmp/runc"
    curl -fLsS --retry 2 -o "$GUEST_RUNC_BIN" "https://github.com/opencontainers/runc/releases/download/v1.5.3/runc.$arch"
fi
printf '%s  %s\n' "$checksum" "$GUEST_RUNC_BIN" | sha256sum --check --status
export WORKLOAD_GUEST=1
bash "$here/build-initramfs.sh" "$@"
