#!/usr/bin/env bash
# Build on Linux with headers for the C compiler and modules for the guest kernel.
set -euo pipefail
init_source=$1
shift
if (( $# < 1 || $# > 2 )); then
    echo "Usage: build-initramfs.sh OUTPUT.cpio.gz [GUEST_KERNEL_RELEASE]" >&2
    exit 2
fi
output=$1
release=${2:-$(uname -r)}
sources=$(cd -- "$(dirname -- "$0")" && pwd)
work=$(mktemp -d)
archive=
trap 'rm -rf -- "$work"; if [[ -n "$archive" ]]; then rm -f -- "$archive"; fi' EXIT
archive=$(mktemp -- "${output}.XXXXXX")
mkdir -p "$work/modules"
"${CC:-cc}" -std=c11 -D_GNU_SOURCE -Os -static -Wall -Wextra -Werror -o "$work/init" -I "$sources" "$init_source"

# Capture modprobe errors. A built-in driver contributes no module file.
for driver in virtio_pci vmw_vsock_virtio_transport; do
    modprobe --set-version "$release" --show-depends "$driver" >>"$work/deps"
done
awk '$1 == "insmod" && !seen[$2]++ { print $2 }' "$work/deps" >"$work/files"
n=0
while IFS= read -r module; do
    case "$module" in
        *.ko) cp -- "$module" "$work/modules/$n.ko" ;;
        *.ko.xz) xz -dc -- "$module" >"$work/modules/$n.ko" ;;
        *.ko.zst) zstd -dc -- "$module" >"$work/modules/$n.ko" ;;
        *.ko.gz) gzip -dc -- "$module" >"$work/modules/$n.ko" ;;
        *) echo "Unrecognized kernel module: $module" >&2; exit 1 ;;
    esac
    n=$((n + 1))
done <"$work/files"
(cd "$work" && find init modules -print0 | cpio --quiet --null -o -H newc --owner=0:0 | gzip -n) >"$archive"
mv -- "$archive" "$output"
