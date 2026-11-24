#!/usr/bin/env bash
# Add a guest-side isolation probe to the standard identity initramfs.
set -euo pipefail
here=$(cd -- "$(dirname -- "$0")" && pwd)
output=$(realpath -m -- "${1:?output initramfs path}")
release=${2:-$(uname -r)}
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
"$here/../../../guest/build-initramfs.sh" "$work/base.gz" "$release"
mkdir "$work/root"
cd "$work/root"
gzip -dc ../base.gz | cpio --quiet -id
mv init identity
cc -std=c11 -D_GNU_SOURCE -static -Os -Wall -Wextra -Werror "$here/probe.c" -o init
modprobe --set-version "$release" --show-depends virtiofs >../deps
n=$(find modules -type f -name '*.ko' | wc -l)
while read -r action path _; do
    [[ $action == insmod ]] || continue
    case "$path" in
        *.ko) cat -- "$path" ;;
        *.xz) xz -dc -- "$path" ;;
        *.zst) zstd -dc -- "$path" ;;
        *.gz) gzip -dc -- "$path" ;;
        *) exit 1 ;;
    esac >"modules/$n.ko"
    n=$((n + 1))
done <../deps
find . -print0 | cpio --quiet -o --null -H newc --owner=0:0 | gzip -n >"$output"
