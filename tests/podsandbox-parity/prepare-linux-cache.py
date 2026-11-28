#!/usr/bin/env python3
"""Seed the isolated Linux CRI test cache from digest-pinned Docker Hub images."""

import hashlib
import json
import re
import sys
from pathlib import Path
from urllib.request import Request, urlopen


ACCEPT = ", ".join((
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
))
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
REFERENCE = re.compile(r"docker\.io/(library/[a-z0-9-]+)@(sha256:[0-9a-f]{64})")


def registry_image(reference, image_dir, architecture):
    match = REFERENCE.fullmatch(reference)
    if not match:
        raise ValueError(f"Expected a digest-pinned official Docker Hub image: {reference}")
    repo, index_digest = match.groups()
    token_url = f"https://auth.docker.io/token?service=registry.docker.io&scope=repository:{repo}:pull"
    with urlopen(token_url, timeout=30) as auth:
        token = json.load(auth)["token"]

    def fetch(kind, digest, size=None):
        if not DIGEST.fullmatch(digest):
            raise ValueError(f"Invalid descriptor digest: {digest}")
        request = Request(f"https://registry-1.docker.io/v2/{repo}/{kind}/{digest}")
        request.add_unredirected_header("Authorization", f"Bearer {token}")
        request.add_header("Accept", ACCEPT)
        with urlopen(request, timeout=90) as response:
            data = response.read()
        if f"sha256:{hashlib.sha256(data).hexdigest()}" != digest or (size is not None and len(data) != size):
            raise ValueError(f"Descriptor verification failed for {repo} {digest}")
        return data

    index = json.loads(fetch("manifests", index_digest))
    descriptor = next(m for m in index["manifests"] if m.get("platform", {}).get("os") == "linux"
                      and m["platform"].get("architecture") == architecture)
    manifest_digest = descriptor["digest"]
    manifest = json.loads(fetch("manifests", manifest_digest, descriptor["size"]))
    config_digest = manifest["config"]["digest"]
    config = json.loads(fetch("blobs", config_digest, manifest["config"]["size"]))
    if (config["os"], config["architecture"]) != ("linux", architecture):
        raise ValueError(f"Wrong platform in {repo} image config")

    for layer in manifest["layers"]:
        digest = layer["digest"]
        if not DIGEST.fullmatch(digest):
            raise ValueError(f"Invalid layer digest: {digest}")
        path = image_dir / "layers" / f"{digest.replace(':', '-')}.blob"
        data = path.read_bytes() if path.exists() else fetch("blobs", digest, layer["size"])
        if len(data) != layer["size"] or f"sha256:{hashlib.sha256(data).hexdigest()}" != digest:
            raise ValueError(f"Layer verification failed for {digest}")
        if not path.exists():
            path.write_bytes(data)

    cached = json.dumps({"manifest": manifest, "config": config, "image_id": config_digest,
                         "manifest_digest": manifest_digest})
    for digest in (index_digest, manifest_digest):
        key = re.sub(r"[^A-Za-z0-9._-]", "_", f"docker.io/{repo}@{digest}")
        (image_dir / "manifests" / f"{key}.json").write_text(cached)
    print(json.dumps({"image": reference, "platform": f"linux/{architecture}", "manifest": manifest_digest}))


if __name__ == "__main__":
    if len(sys.argv) != 4 or sys.argv[3] not in {"amd64", "arm64"}:
        sys.exit("Usage: prepare-linux-cache.py MANIFEST.json CACHE_DIRECTORY amd64|arm64")
    fixture = json.loads(Path(sys.argv[1]).read_text())
    cache = Path(sys.argv[2])
    for child in ("manifests", "layers"):
        (cache / child).mkdir(parents=True, exist_ok=True)
    for image in dict.fromkeys(member["image"] for member in fixture["containers"]):
        registry_image(image, cache, sys.argv[3])
