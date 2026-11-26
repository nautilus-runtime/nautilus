#!/usr/bin/env bash
# Prepare a verification host for nautilus build & test.
#
# Target: any Linux host (x86_64 or aarch64) where you have root or
# passwordless sudo; the script installs the toolchain, crictl, CNI plugins
# and protoc, and sets up swap on small-memory machines.
#
# Usage (from the repo checkout):
#   ssh <host> 'bash -s' < scripts/setup_remote_env.sh
#   bash scripts/setup_remote_env.sh --protoc-only  # install for the current user
#
# Hosts behind restricted networks cannot reach GitHub directly: export
# NAUTILUS_PROXY (e.g. http://<proxy-host>:<port>) before piping this script
# there. Hosts with direct egress leave it unset.
set -euo pipefail

PROXY="${NAUTILUS_PROXY:-}"
[ -n "$PROXY" ] && export https_proxy="$PROXY" http_proxy="$PROXY"

CRICTL_VERSION="v1.31.1"
CNI_VERSION="v1.5.1"
PROTOC_VERSION="29.3"

ARCH="$(uname -m)"
case "$ARCH" in
    x86_64)
        GOARCH="amd64"
        PROTOC_ARCH="linux-x86_64"
        PROTOC_SHA256="3e866620c5be27664f3d2fa2d656b5f3e09b5152b42f1bedbf427b333e90021a"
        ;;
    aarch64|arm64)
        GOARCH="arm64"
        PROTOC_ARCH="linux-aarch_64"
        PROTOC_SHA256="6427349140e01f06e049e707a58709a4f221ae73ab9a0425bc4a00c8d0e1ab32"
        ;;
    *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac

# Optional command prefix (sudo) is used only for the system-wide install.
install_protoc() (
    local prefix=$1 tmp archive
    shift
    tmp=$(mktemp -d)
    trap 'rm -rf -- "$tmp"' EXIT
    archive="protoc-${PROTOC_VERSION}-${PROTOC_ARCH}.zip"
    curl -fsSL "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/${archive}" -o "$tmp/$archive"
    (cd "$tmp" && printf '%s  %s\n' "$PROTOC_SHA256" "$archive" | sha256sum -c -)
    python3 -m zipfile -e "$tmp/$archive" "$tmp/unpacked"
    "$@" install -d "$prefix/bin" "$prefix/include"
    "$@" install -m 0755 "$tmp/unpacked/bin/protoc" "$prefix/bin/protoc"
    "$@" cp -R "$tmp/unpacked/include/." "$prefix/include/"
    "$prefix/bin/protoc" --version
)

if [ "$#" -gt 0 ]; then
    if [ "$#" != 1 ] || [ "$1" != --protoc-only ]; then
        echo "Usage: $0 [--protoc-only]" >&2
        exit 2
    fi
    install_protoc "$HOME/.local/share/nautilus/protoc-$PROTOC_VERSION"
    exit
fi

if [ "$(id -u)" != "0" ] && ! sudo -n true 2>/dev/null; then
    echo "need root or passwordless sudo" >&2
    exit 1
fi

echo "== swap (small build hosts need headroom; >=4G RAM hosts skip) =="
MEM_KB=$(awk '/MemTotal/ {print $2}' /proc/meminfo)
if [ "$MEM_KB" -lt 4000000 ]; then
    if ! swapon --show | grep -q '/swapfile'; then
        sudo fallocate -l 2G /swapfile
        sudo chmod 600 /swapfile
        sudo mkswap -q /swapfile
        sudo swapon /swapfile
        echo "swap created"
    else
        echo "swap already active"
    fi
else
    echo "RAM ${MEM_KB} kB — swap not needed"
fi
free -h | sed -n '1,2p'

echo "== rust toolchain =="
# NOTE: an old rustup may already live in ~/.cargo with a stale DEFAULT
# toolchain (e.g. an old pinned stable while a newer one exists). Always refresh.
export PATH="$HOME/.cargo/bin:$PATH"
if [ ! -x "$HOME/.cargo/bin/rustup" ]; then
    curl -fsSL https://sh.rustup.rs -o /tmp/rustup-init.sh
    sh /tmp/rustup-init.sh -y --profile minimal --default-toolchain stable > /dev/null
    echo "rustup installed"
else
    rustup update stable
    rustup default stable
fi
rustup target add wasm32-unknown-unknown
rustc --version
cargo --version

echo "== crictl ($GOARCH) =="
if ! command -v crictl > /dev/null 2>&1; then
    curl -fsSL -o /tmp/crictl.tgz \
        "https://github.com/kubernetes-sigs/cri-tools/releases/download/${CRICTL_VERSION}/crictl-${CRICTL_VERSION}-linux-${GOARCH}.tar.gz"
    sudo tar -C /usr/local/bin -xzf /tmp/crictl.tgz crictl
    rm -f /tmp/crictl.tgz
    echo "crictl installed"
fi
crictl --version

echo "== runc sanity =="
runc --version | head -1

echo "== CNI plugins ($GOARCH, reference binaries for pod networking) =="
if [ ! -x /opt/cni/bin/bridge ]; then
    cd /tmp
    curl -fsSLO "https://github.com/containernetworking/plugins/releases/download/${CNI_VERSION}/cni-plugins-linux-${GOARCH}-${CNI_VERSION}.tgz"
    sudo mkdir -p /opt/cni/bin
    sudo tar -C /opt/cni/bin -xzf "cni-plugins-linux-${GOARCH}-${CNI_VERSION}.tgz"
    rm -f "cni-plugins-linux-${GOARCH}-${CNI_VERSION}.tgz"
fi
ls /opt/cni/bin | head -3

echo "== protoc >= 29 (upstream CRI proto uses debug_redact; apt's is too old) =="
if ! command -v protoc > /dev/null 2>&1 || [ "$(protoc --version | awk '{print $2}' | cut -d. -f1)" -lt 29 ]; then
    install_protoc /usr/local sudo
fi
protoc --version

echo "== container networking sysctls =="
# Pod networking needs forwarding; bridge-netfilter only if filtering desired.
sudo sysctl -w net.ipv4.ip_forward=1 > /dev/null
# Port publishing DNATs loopback-sourced traffic to pod IPs; without this the
# kernel treats the 127/8 source as martian and blackholes it.
sudo sysctl -w net.ipv4.conf.all.route_localnet=1 > /dev/null
grep -qs 'route_localnet' /etc/sysctl.d/99-nautilus.conf 2>/dev/null || \
    printf 'net.ipv4.conf.all.route_localnet = 1\n' | sudo tee -a /etc/sysctl.d/99-nautilus.conf > /dev/null
if [ "$(sysctl -n net.ipv4.ip_forward)" != "1" ] || ! grep -qs 'ip_forward' /etc/sysctl.d/99-nautilus.conf 2>/dev/null; then
    printf 'net.ipv4.ip_forward = 1\n' | sudo tee /etc/sysctl.d/99-nautilus.conf > /dev/null
fi
sysctl -n net.ipv4.ip_forward

echo "== install client/daemon into PATH =="
# Symlink the release binaries (if a build exists) so `nautilus` / `nautilusd`
# work from any shell. Idempotent; skipped when nothing is built yet.
for b in nautilus nautilusd; do
    if [ -x "target/${TARGET:-aarch64-unknown-linux-gnu}/release/$b" ]; then
        sudo ln -sfn "$(pwd)/target/${TARGET:-aarch64-unknown-linux-gnu}/release/$b" "/usr/local/bin/$b"
    fi
done
command -v nautilus >/dev/null && echo "nautilus: $(command -v nautilus)"
command -v nautilusd >/dev/null && echo "nautilusd: $(command -v nautilusd)"

echo "SETUP-OK"
