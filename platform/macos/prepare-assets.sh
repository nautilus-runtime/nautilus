#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 1 ]]; then
    printf 'Usage: %s ASSET_DIRECTORY\n' "$0" >&2
    exit 2
fi

mkdir -p -- "$1"
destination=$(cd -- "$1" && pwd -P)
scratch=$(mktemp -d "$destination/.nautilus-assets.XXXXXXXX")
trap 'rm -rf -- "$scratch"' EXIT

kernel_archive_url=https://github.com/kata-containers/kata-containers/releases/download/4.3.1/kata-static-4.3.1-arm64.tar.zst
# Apple container 1.7.1 specifies this archive digest and kernel member.
kernel_archive_sha=2c5bf7cde334ad5307d482f45cceda496f91c9b6b96f5278c47f4f4eac1bbae6
kernel_member=opt/kata/share/kata-containers/vmlinux-6.18.56-208-debug
runc_url=https://github.com/opencontainers/runc/releases/download/v1.5.3/runc.arm64
runc_sha=07b52aee923cc432c38fe4fb6809a08fc6471994efed650af3635350588a20be

curl --fail --silent --show-error --location --retry 2 --connect-timeout 15 \
    --output "$scratch/kata.tar.zst" "$kernel_archive_url"
printf '%s  %s\n' "$kernel_archive_sha" "$scratch/kata.tar.zst" | shasum -a 256 -c -
zstd --decompress --stdout "$scratch/kata.tar.zst" \
    | tar -xOf - --include "$kernel_member" --include "./$kernel_member" > "$scratch/kernel"
test -s "$scratch/kernel"

curl --fail --silent --show-error --location --retry 2 --connect-timeout 15 \
    --output "$scratch/runc.arm64" "$runc_url"
printf '%s  %s\n' "$runc_sha" "$scratch/runc.arm64" | shasum -a 256 -c -

python3 - "$scratch" <<'PY'
import hashlib
import json
from pathlib import Path
import sys
import tarfile
from urllib.request import Request, urlopen

scratch = Path(sys.argv[1])
repository = "apple/containerization/vminit"
# The only layer of the release's linux/arm64 vminit:0.57.0 manifest.
layer_sha = "dbfb1148e31375e6ed728c67ddd7a62509b13161649f301297a18b8914cf9c7f"
with urlopen(f"https://ghcr.io/token?scope=repository:{repository}:pull", timeout=30) as response:
    token = json.load(response)["token"]
request = Request(
    f"https://ghcr.io/v2/{repository}/blobs/sha256:{layer_sha}",
    headers={"Authorization": f"Bearer {token}"},
)

layer = scratch / "vminit.tar.gz"
digest = hashlib.sha256()
with urlopen(request, timeout=120) as response, layer.open("wb") as download:
    while chunk := response.read(1024 * 1024):
        digest.update(chunk)
        download.write(chunk)
if digest.hexdigest() != layer_sha:
    raise SystemExit("Apple vminit layer SHA-256 mismatch")

# Preserve the upstream guest filesystem metadata and add the OCI runtime.
with tarfile.open(layer, "r:gz") as source, tarfile.open(scratch / "init-rootfs.tar.gz", "w:gz", compresslevel=6) as rootfs:
    names = {member.name.removeprefix("./") for member in source.getmembers()}
    if not {"sbin/vminitd", "sbin/vmexec"}.issubset(names) or "sbin/runc" in names:
        raise SystemExit("Unexpected Apple vminit layer layout")
    for member in source.getmembers():
        if member.isfile():
            with source.extractfile(member) as contents:
                rootfs.addfile(member, contents)
        else:
            rootfs.addfile(member)
    runc_path = scratch / "runc.arm64"
    runtime = tarfile.TarInfo("./sbin/runc")
    runtime.size = runc_path.stat().st_size
    runtime.mode = 0o755
    with runc_path.open("rb") as executable:
        rootfs.addfile(runtime, executable)
PY

mv -- "$scratch/kernel" "$scratch/init-rootfs.tar.gz" "$destination/"
