"""Measure the first use of a cached image on two dedicated Nautilus endpoints."""
import argparse
import csv
from pathlib import Path
import time
import uuid

from benchmark_cri import Endpoint, METRICS, call, cri, sample


def run(args, endpoints, output):
    image = cri.ImageSpec(image=args.image)
    ids = []
    for endpoint in endpoints.values():
        if "nautilus" not in endpoint.version.runtime_name.lower():
            raise ValueError("both endpoints must be Nautilus")
        if call(endpoint.runtime.ListContainers, cri.ListContainersRequest()).containers:
            raise ValueError("use dedicated endpoints with no containers")
        ids.append(call(endpoint.images.ImageStatus, cri.ImageStatusRequest(image=image)).image.id)
    if not ids[0] or ids[0] != ids[1]:
        raise ValueError("preload the same image on both endpoints")

    writer = csv.writer(output)
    writer.writerow(["implementation", "pair", "pull_ms", *METRICS])
    run_id = uuid.uuid4().hex
    try:
        for pair in range(args.pairs):
            order = ("before", "after") if pair % 2 == 0 else ("after", "before")
            for name in order:
                endpoint = endpoints[name]
                call(endpoint.images.RemoveImage, cri.RemoveImageRequest(image=image))
                if name == "after" and args.snapshot_dir.exists():
                    if any(path.is_dir() for path in args.snapshot_dir.iterdir()):
                        raise RuntimeError("unpacked trees remain in the dedicated snapshot store")
                start = time.perf_counter_ns()
                call(endpoint.images.PullImage, cri.PullImageRequest(image=image))
                pull_ms = round((time.perf_counter_ns() - start) / 1e6, 3)
                writer.writerow([name, pair, pull_ms, *sample(endpoint, args.image, run_id, args.workdir)])
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
    parser.add_argument("--image", required=True)
    parser.add_argument("--pairs", type=int, default=15)
    args = parser.parse_args()
    if args.before.resolve() == args.after.resolve() or args.pairs < 1 or "@sha256:" not in args.image:
        parser.error("use distinct sockets, positive pairs and a pinned image digest")
    if not args.workdir.is_absolute() or not args.snapshot_dir.is_dir():
        parser.error("use an absolute shared workdir and the existing candidate snapshot directory")
    with args.output.open("x", newline="") as output:
        endpoints = {}
        try:
            for name in ("before", "after"):
                endpoints[name] = Endpoint(getattr(args, name))
            run(args, endpoints, output)
        finally:
            for endpoint in endpoints.values():
                endpoint.channel.close()


if __name__ == "__main__":
    main()
