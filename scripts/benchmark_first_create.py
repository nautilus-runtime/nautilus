"""Measure the first use of a cached image on dedicated Nautilus builds and an optional containerd control."""
import argparse
import csv
from contextlib import contextmanager, nullcontext
from itertools import permutations
import json
from pathlib import Path
import subprocess
import time
import uuid

from benchmark_cri import Endpoint, METRICS, call, cri, sample


@contextmanager
def containerd_cache(socket, image, config_id, transfer):
    """Reset unpacked state and, for a registry transfer, wait for content GC."""
    def ctr(*args):
        return subprocess.check_output(
            ["ctr", "--address", str(socket), "--namespace", "k8s.io", *args], text=True)

    manifest_digest = image.split("@", 1)[1]
    manifest = json.loads(ctr("content", "get", manifest_digest))
    config = json.loads(ctr("content", "get", config_id))
    if manifest["config"]["digest"] != config_id or len(manifest["layers"]) != 1 or len(config["rootfs"]["diff_ids"]) != 1:
        raise ValueError("use a single-platform, single-layer image for the containerd reset")
    chain = config["rootfs"]["diff_ids"][0]
    layer = manifest["layers"][0]["digest"]

    def snapshots():
        return {line.split()[0] for line in ctr("snapshots", "list").splitlines()[1:] if line.strip()}

    required = {manifest_digest, config_id, layer}
    if transfer:
        def wait_for_gc():
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if not required.intersection(ctr("content", "ls", "-q").split()) and chain not in snapshots():
                    return
                time.sleep(0.05)
            raise RuntimeError("workload content or snapshot is still referenced in the containerd fixture")
        yield wait_for_gc, layer
        return

    def reset():
        ctr("content", "label", config_id, "containerd.io/gc.ref.snapshot.overlayfs=")
        if chain in snapshots():
            try:
                ctr("snapshots", "remove", chain)
            except subprocess.CalledProcessError:
                if chain in snapshots():
                    raise
        present = set(ctr("content", "ls", "-q").splitlines())
        if chain in snapshots() or not required <= present:
            raise RuntimeError("containerd reset did not preserve exactly the required cache state")

    labels = ctr("content", "label", manifest_digest).strip().split(",")
    already_pinned = any(label.startswith("containerd.io/gc.root=") for label in labels)
    if not already_pinned:
        ctr("content", "label", manifest_digest, "containerd.io/gc.root=benchmark")
    try:
        yield reset, layer
    finally:
        if not already_pinned:
            ctr("content", "label", manifest_digest, "containerd.io/gc.root=")


def clear_nautilus_content(store, ref, layer):
    # Only the requested manifest and layer in an explicitly disposable store.
    key = "".join(c if c.isalnum() or c in "._-" else "_" for c in ref)
    manifest = store / "manifests" / (key + ".json")
    cached = json.loads(manifest.read_bytes())
    if (cached["manifest_digest"] != ref.split("@", 1)[1]
            or [entry["digest"] for entry in cached["manifest"]["layers"]] != [layer]):
        raise ValueError("specified image store does not contain the benchmark manifest")
    (store / "layers" / (layer.replace(":", "-") + ".blob")).unlink()
    manifest.unlink()


def run(args, endpoints, output):
    image = cri.ImageSpec(image=args.image)
    ids = set()
    for name, endpoint in endpoints.items():
        expected = "containerd" if name == "containerd" else "nautilus"
        if expected not in endpoint.version.runtime_name.lower():
            raise ValueError(f"unexpected runtime at {name}")
        if (call(endpoint.runtime.ListContainers, cri.ListContainersRequest()).containers
                or call(endpoint.runtime.ListPodSandbox, cri.ListPodSandboxRequest()).items):
            raise ValueError("use dedicated endpoints with no pods or containers")
        ids.add(call(endpoint.images.ImageStatus, cri.ImageStatusRequest(image=image)).image.id)
    if len(ids) != 1 or "" in ids:
        raise ValueError("preload the same image on every endpoint")
    config_id = ids.pop()
    stores = dict(zip(("before", "after"), args.transfer_image_dirs or []))
    cache = (containerd_cache(args.containerd, args.image, config_id, bool(stores))
             if args.containerd else nullcontext((None, None)))
    dirs = {"before": args.before_snapshot_dir, "after": args.snapshot_dir}
    orders = list(permutations(endpoints))
    writer = csv.writer(output)
    writer.writerow(["implementation", "pair", "pull_ms", *METRICS, "pull_ready_ms"])
    run_id = uuid.uuid4().hex
    try:
        with cache as (reset_containerd, layer):
            for pair in range(args.pairs):
                for name in orders[pair % len(orders)]:
                    endpoint = endpoints[name]
                    call(endpoint.images.RemoveImage, cri.RemoveImageRequest(image=image))
                    if call(endpoint.images.ImageStatus, cri.ImageStatusRequest(image=image)).image.id:
                        raise RuntimeError(f"image record remains on {name}")
                    if name == "containerd":
                        reset_containerd()
                    elif dirs[name] and any(path.is_dir() for path in dirs[name].iterdir()):
                        raise RuntimeError(f"unpacked trees remain on {name}")
                    if name in stores:
                        clear_nautilus_content(stores[name], args.image, layer)
                    *stages, pull, total = sample(endpoint, args.image, run_id, args.workdir, pull_first=True)
                    if call(endpoint.images.ImageStatus, cri.ImageStatusRequest(image=image)).image.id != config_id:
                        raise RuntimeError(f"image identity changed on {name}")
                    writer.writerow([name, pair, pull, *stages, total])
                    output.flush()
    finally:
        for endpoint in endpoints.values():
            endpoint.cleanup(*endpoint.objects(run_id))
            if any(endpoint.objects(run_id)):
                raise RuntimeError("benchmark objects remain on endpoint")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ("before", "after", "snapshot-dir", "workdir", "output"):
        parser.add_argument("--" + option, type=Path, required=True)
    parser.add_argument("--containerd", type=Path, help="dedicated single-layer overlayfs control socket")
    parser.add_argument("--transfer-image-dirs", type=Path, nargs=2, metavar=("BEFORE", "AFTER"),
                        help="evict workload bytes in these two disposable Nautilus image stores before each pull")
    parser.add_argument("--before-snapshot-dir", type=Path, help="baseline unpacked snapshot store")
    parser.add_argument("--image", required=True)
    parser.add_argument("--pairs", type=int, default=15)
    args = parser.parse_args()
    names = ["before", "after"] + (["containerd"] if args.containerd else [])
    if len({getattr(args, name).resolve() for name in names}) != len(names) or args.pairs < 1 or "@sha256:" not in args.image:
        parser.error("use distinct sockets, positive pairs and a pinned image digest")
    if not args.workdir.is_absolute() or not args.snapshot_dir.is_dir():
        parser.error("use an absolute shared workdir and the existing candidate snapshot directory")
    if args.containerd and not args.before_snapshot_dir:
        parser.error("provide --before-snapshot-dir for the three-runtime comparison")
    if args.before_snapshot_dir and not args.before_snapshot_dir.is_dir():
        parser.error("baseline snapshot store must exist")
    if args.transfer_image_dirs:
        stores = {p.resolve() for p in args.transfer_image_dirs}
        if not args.containerd or len(stores) != 2 or not all((p / "manifests").is_dir() for p in stores):
            parser.error("registry transfers need containerd and two distinct disposable Nautilus image stores")
    with args.output.open("x", newline="") as output:
        endpoints = {}
        try:
            for name in names:
                endpoints[name] = Endpoint(getattr(args, name))
            run(args, endpoints, output)
        finally:
            for endpoint in endpoints.values():
                endpoint.channel.close()


if __name__ == "__main__":
    main()
