#!/usr/bin/env bash
# Run as root on a KVM host with the matching guest kernel/initramfs.
set -euo pipefail
: "${VMM:?path to Cloud Hypervisor}" "${VIRTIOFSD:?path to virtiofsd}" "${KERNEL:?path to guest kernel}" "${INITRAMFS:?path to guest initramfs}" "${BUSYBOX:?static workload busybox}"
here=$(cd -- "$(dirname -- "$0")" && pwd)
work=$(mktemp -d /tmp/nt-oci-test.XXXXXX)
vmm_pid=
fs_pid=
cleanup() {
    result=$?
    if [[ -n "$vmm_pid" ]]; then kill "$vmm_pid" 2>/dev/null || true; wait "$vmm_pid" 2>/dev/null || true; fi
    if [[ -n "$fs_pid" ]]; then kill "$fs_pid" 2>/dev/null || true; wait "$fs_pid" 2>/dev/null || true; fi
    if (( result != 0 )); then tail -c 8192 "$work"/{serial,vmm.log,fs.log} >&2 || true; fi
    rm -rf -- "$work"
}
trap cleanup EXIT
mkdir "$work/share"
"$VIRTIOFSD" --socket-path "$work/fs.sock" --shared-dir "$work/share" --xattr >"$work/fs.log" 2>&1 &
fs_pid=$!
for _ in {1..100}; do [[ -S "$work/fs.sock" ]] && break; sleep .05; done
sandbox=$(cat /proc/sys/kernel/random/uuid)
nonce=$(cat /proc/sys/kernel/random/uuid)
"$VMM" --kernel "$KERNEL" --initramfs "$INITRAMFS" --cmdline "console=ttyS0 rdinit=/init panic=1 nautilus.sandbox_id=$sandbox nautilus.nonce=$nonce" \
  --vsock "cid=5,socket=$work/vsock" --memory size=512M,shared=on --cpus boot=2 --fs "tag=podfs,socket=$work/fs.sock" \
  --serial "file=$work/serial" --console off </dev/null >"$work/vmm.log" 2>&1 &
vmm_pid=$!
python3 "$here/oci.py" "$work/vsock" "$work/share" "$BUSYBOX"
