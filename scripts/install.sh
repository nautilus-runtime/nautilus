#!/usr/bin/env bash
set -euo pipefail

fail() {
    printf '%s\n' "$*" >&2
    exit 1
}

[[ $(uname -s) == Linux && -d /run/systemd/system ]] || fail "Nautilus requires Linux with systemd."
case "$(uname -m)" in
    x86_64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) fail "Unsupported architecture: $(uname -m)" ;;
esac

releases=https://github.com/nautilus-runtime/nautilus/releases
if [[ $# == 0 ]]; then
    download="$releases/latest/download"
elif [[ $# == 1 && $1 =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.-]+)?$ ]]; then
    download="$releases/download/$1"
else
    fail "Usage: install.sh [vMAJOR.MINOR.PATCH]"
fi

asset="nautilus-linux-$arch.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
for file in "$asset" "$asset.sha256"; do
    curl --proto '=https' --proto-redir '=https' -fsSL --retry 3 "$download/$file" -o "$tmp/$file"
done
read -r expected _ < "$tmp/$asset.sha256"
checksum=$(sha256sum "$tmp/$asset")
[[ "$expected" =~ ^[a-f0-9]{64}$ && "${checksum%% *}" == "$expected" ]] || fail "Checksum mismatch for $asset."
tar --no-same-owner -xzf "$tmp/$asset" -C "$tmp"

bin="$tmp/usr/local/bin"
config=/etc/nautilus/config.toml
unit=nautilusd.service
system_unit="/etc/systemd/system/$unit"
for file in "$bin/nautilus" "$bin/nautilusd" "$tmp$config" "$tmp$system_unit"; do
    [[ -f "$file" && ! -L "$file" ]] || fail "Invalid release package. Missing regular file: $file"
done

root=()
if (( EUID != 0 )); then
    command -v sudo >/dev/null || fail "Install as root or with sudo."
    root=(sudo)
fi

cni_dir() {
    local dir
    for dir in /opt/cni/bin /usr/lib/cni /usr/libexec/cni; do
        if [[ -x "$dir/bridge" && -x "$dir/loopback" && -x "$dir/host-local" ]]; then
            printf '%s\n' "$dir"
            return
        fi
    done
    return 1
}

dependencies_ready() {
    command -v ip >/dev/null && command -v iptables >/dev/null && cni_dir >/dev/null
}

lock=/run/nautilus-install.lock
"${root[@]}" touch "$lock"
exec 9<"$lock"
flock -n 9 || fail "Another Nautilus installation is in progress."

existing=false
if "${root[@]}" test -e "$config" || systemctl cat "$unit" >/dev/null 2>&1; then
    existing=true
fi
if [[ $existing == false ]] && ! dependencies_ready; then
    command -v apt-get >/dev/null || fail "Install iproute2, iptables and the bridge, loopback and host-local CNI plugins first."
    "${root[@]}" apt-get update
    "${root[@]}" apt-get install -y --no-install-recommends iproute2 iptables containernetworking-plugins
    dependencies_ready || fail "Required CNI plugins or networking tools were not found."
fi

"$bin/nautilus" --help >/dev/null
"$bin/nautilusd" --help >/dev/null
"${root[@]}" install -d /usr/local/bin /etc/nautilus /etc/systemd/system /var/lib/nautilus /var/log/nautilus /var/run/nautilus
for name in nautilus nautilusd; do
    # Rename the finished binary so an active daemon keeps its old executable until restarted.
    "${root[@]}" install -m 0755 "$bin/$name" "/usr/local/bin/.$name.install"
    "${root[@]}" mv -fT "/usr/local/bin/.$name.install" "/usr/local/bin/$name"
done
if ! "${root[@]}" test -e "$config"; then
    if plugins=$(cni_dir); then
        sed "s|^cni_bin_dir = .*|cni_bin_dir = \"$plugins\"|" "$tmp$config" > "$tmp/config.toml"
        "${root[@]}" install -m 0644 "$tmp/config.toml" "$config"
    else
        "${root[@]}" install -m 0644 "$tmp$config" "$config"
    fi
fi
if ! systemctl cat "$unit" >/dev/null 2>&1; then
    "${root[@]}" install -m 0644 "$tmp$system_unit" "$system_unit"
fi
"${root[@]}" systemctl daemon-reload
if [[ $existing == true ]]; then
    printf '%s\n' "Updated Nautilus. Restart nautilusd after draining workloads."
else
    "${root[@]}" systemctl enable --now "$unit"
    printf '%s\n' "Installed Nautilus. Check the service with sudo nautilus version."
fi
