#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 5 || $EUID != 0 ]]; then
    echo 'Usage (as root): run-isolated-linux.sh NAUTILUSD ASSET_DIR GUEST_KERNEL IMAGE_CACHE REPORT_DIRECTORY' >&2
    exit 2
fi

suite=$(cd -- "$(dirname -- "$0")" && pwd)
daemon=$(realpath -- "$1")
assets=$(realpath -- "$2")
kernel=$(realpath -- "$3")
cache=$(realpath -- "$4")
report=$(realpath -- "$5")
work=$(mktemp -d /tmp/ntm3-run.XXXXXXXX)
echo "Isolated CRI workspace: $work"

mkdir -p -- "$work/cni" "$work/images"
cp -a -- "$cache/manifests" "$cache/layers" "$work/images/"
"$daemon" --dump-config "$work/default.toml"
python3 - "$work" "$assets" "$kernel" <<'PY'
import json
import sys
from pathlib import Path

work, assets, kernel = map(Path, sys.argv[1:])

overrides = {
    "runtime": {"state_dir": work / "state", "max_containers": 16},
    "runtime.runc": {"root": work / "runc", "systemd_cgroup": True},
    "runtime.sandboxes": {"vm_workloads_enabled": True},
    "storage": {key: work / suffix for key, suffix in (
        ("root_dir", "storage"), ("image_dir", "images"),
        ("container_dir", "containers"), ("tmp_dir", "tmp"),
    )},
    "storage.gc": {"enable": False},
    "network": {"cni_config_dir": work / "cni", "cni_bin_dir": assets / "cni",
                "netns_dir": work / "netns", "bridge_name": "ntm3",
                "subnet": "192.0.2.0/24", "gateway": "192.0.2.1"},
    "network.dns": {"nameservers": [], "search_domains": [], "options": []},
    "network.firewall": {"enable_iptables": False, "masquerade": False},
    "cri": {"listen_address": f"unix://{work}/cri.sock"},
    "auth": {"config_file": work / "no-registry-credentials.json"},
}

def toml(value):
    return json.dumps(str(value) if isinstance(value, Path) else value, ensure_ascii=False)

lines = []
section = ""
skip_array = False
for line in (work / "default.toml").read_text().splitlines():
    if skip_array:
        skip_array = line.strip() != "]"
        continue
    if line.startswith("["):
        section = line.strip("[]")
    key, _, value = line.partition("=")
    key = key.strip()
    if key in overrides.get(section, {}):
        skip_array = value.strip() == "["
        line = f"{key} = {toml(overrides[section].pop(key))}"
    lines.append(line)
if any(overrides.values()):
    raise RuntimeError(f"Unexpected daemon defaults: {overrides}")
lines.append("\n[runtime.sandboxes.vm]")
for key, path in {"vmm": assets / "cloud-hypervisor", "kernel": kernel,
                  "initramfs": assets / "initramfs.cpio.gz", "virtiofsd": assets / "virtiofsd"}.items():
    lines.append(f"{key} = {toml(path)}")
(work / "config.toml").write_text("\n".join(lines) + "\n")

chain = {"cniVersion": "0.4.0", "name": "ntm3", "plugins": [
    {"type": "loopback"},
    {"type": "bridge", "bridge": "ntm3", "isGateway": False, "ipMasq": False, "portIsolation": True,
     "ipam": {"type": "host-local", "subnet": "192.0.2.0/24", "dataDir": str(work / "ipam")}},
]}
(work / "cni" / "m3.conflist").write_text(json.dumps(chain) + "\n")
PY

unshare --net -- bash -se -- "$daemon" "$assets" "$suite" "$work" "$report" <<'SH'
set -euo pipefail
daemon=$1 assets=$2 suite=$3 work=$4 report=$5
endpoint="unix://$work/cri.sock"
cri=("$assets/crictl" --runtime-endpoint "$endpoint" --image-endpoint "$endpoint")
ip link set lo up
"$daemon" --config "$work/config.toml" >"$report/daemon.log" 2>&1 &
daemon_pid=$!
trap 'kill "$daemon_pid" 2>/dev/null || :; wait "$daemon_pid" 2>/dev/null || :' EXIT

ready=false
for attempt in $(seq 1 90); do
    if "${cri[@]}" --timeout 2s info 2>/dev/null | python3 -c 'import json,sys; sys.exit(not any(x.get("name") == "vm" for x in json.load(sys.stdin).get("runtimeHandlers", [])))' 2>/dev/null; then
        ready=true
        break
    fi
    kill -0 "$daemon_pid" 2>/dev/null || break
    sleep 1
done
if [[ $ready != true ]]; then
    echo "Test daemon did not publish the VM handler. See $report/daemon.log" >&2
    exit 1
fi

test -z "$(ip route show default)"
python3 "$suite/verify-linux.py" --crictl "$assets/crictl" --runtime-endpoint "$endpoint" 2>&1 | tee "$report/linux.log"
for command in 'pods' 'ps -a'; do
    if [[ $command == pods ]]; then ids=$("${cri[@]}" pods -q)
    else ids=$("${cri[@]}" ps -a -q); fi
    test -z "$ids"
done
remaining=$(find "$work/state/vms" "$work/netns" -mindepth 1 -maxdepth 1 ! -name '.*' -print)
test -z "$remaining"
leases=$(find "$work/ipam" -type f -name '192.0.2.*' -print)
test -z "$leases"
echo 'PASS: no remaining CRI pods, containers, VM state, network namespaces or IPAM leases' | tee -a "$report/linux.log"
SH

if findmnt --list --noheadings --output TARGET | grep -F -- "$work/"; then
    echo "Retained $work because a test mount remains" >&2
    exit 1
fi
rm -rf -- "$work"
