"""Compare dedicated Linux CRI endpoints. See docs/PERFORMANCE.md."""
import argparse
import csv
from itertools import permutations
import json
import math
import tempfile
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import grpc
import api_pb2 as cri
from api_pb2_grpc import ImageServiceStub, RuntimeServiceStub


METRICS = ("pod_ms", "create_ms", "start_ms", "create_start_ms", "ready_ms")
LABEL = "io.nautilus.benchmark.run"


def call(method, request):
    return method(request, timeout=45)


class Endpoint:
    def __init__(self, path):
        self.channel = grpc.insecure_channel("unix://" + str(path.resolve()),
                                             options=[("grpc.default_authority", "localhost")])
        self.runtime = RuntimeServiceStub(self.channel)
        self.images = ImageServiceStub(self.channel)
        self.version = call(self.runtime.Version, cri.VersionRequest(version="0.1.0"))

    def objects(self, run):
        labels = {LABEL: run}
        containers = call(self.runtime.ListContainers, cri.ListContainersRequest(
            filter=cri.ContainerFilter(label_selector=labels))).containers
        pods = call(self.runtime.ListPodSandbox, cri.ListPodSandboxRequest(
            filter=cri.PodSandboxFilter(label_selector=labels))).items
        # Some CRI implementations ignore label_selector. Verify ownership locally.
        return ([c.id for c in containers if c.labels.get(LABEL) == run],
                [p.id for p in pods if p.labels.get(LABEL) == run])

    def cleanup(self, containers, pods):
        for cid in containers:
            call(self.runtime.StopContainer, cri.StopContainerRequest(container_id=cid, timeout=0))
            call(self.runtime.RemoveContainer, cri.RemoveContainerRequest(container_id=cid))
        for pid in pods:
            call(self.runtime.StopPodSandbox, cri.StopPodSandboxRequest(pod_sandbox_id=pid))
            call(self.runtime.RemovePodSandbox, cri.RemovePodSandboxRequest(pod_sandbox_id=pid))


def sample(endpoint, image, run, base, *, pull_first=False):
    runtime = endpoint.runtime
    uid = uuid.uuid4().hex
    labels = {LABEL: run}
    namespaces = cri.NamespaceOption(network=cri.NODE, pid=cri.CONTAINER, ipc=cri.NODE)
    with tempfile.TemporaryDirectory(dir=base) as name:
        path = Path(name)
        pod = cri.PodSandboxConfig(
            metadata=cri.PodSandboxMetadata(name="bench-" + uid, namespace="cri-bench", uid=uid),
            labels=labels, log_directory=name,
            linux=cri.LinuxPodSandboxConfig(cgroup_parent="system.slice", security_context=cri.LinuxSandboxSecurityContext(
                namespace_options=namespaces)))
        config = cri.ContainerConfig(
            metadata=cri.ContainerMetadata(name="worker"), image=cri.ImageSpec(image=image),
            command=["/bin/sh", "-c", "echo ready > /probe/ready && exec sleep 180"],
            mounts=[cri.Mount(container_path="/probe", host_path=name)], labels=labels,
            log_path="worker.log", linux=cri.LinuxContainerConfig(
                security_context=cri.LinuxContainerSecurityContext(namespace_options=namespaces)))
        containers, pods = [], []
        try:
            if pull_first:
                request = cri.PullImageRequest(image=config.image)
                pull_begin = time.perf_counter_ns()
                call(endpoint.images.PullImage, request)
            t0 = time.perf_counter_ns()
            pid = call(runtime.RunPodSandbox, cri.RunPodSandboxRequest(config=pod)).pod_sandbox_id
            t1 = time.perf_counter_ns()
            pods.append(pid)
            cid = call(runtime.CreateContainer, cri.CreateContainerRequest(
                pod_sandbox_id=pid, config=config, sandbox_config=pod)).container_id
            t2 = time.perf_counter_ns()
            containers.append(cid)
            call(runtime.StartContainer, cri.StartContainerRequest(container_id=cid))
            t3 = time.perf_counter_ns()
            marker = path / "ready"
            while not marker.exists() or marker.read_text() != "ready\n":
                if time.perf_counter_ns() - t3 > 10_000_000_000:
                    raise RuntimeError("workload readiness deadline exceeded")
                time.sleep(0.001)
            t4 = time.perf_counter_ns()
            status = call(runtime.ContainerStatus, cri.ContainerStatusRequest(container_id=cid)).status
            if status.state != cri.CONTAINER_RUNNING:
                raise RuntimeError("workload did not remain running")
            times = [t1-t0, t2-t1, t3-t2, t3-t1, t4-t0]
            if pull_first:
                times += [t0-pull_begin, t4-pull_begin]
            return [round(v / 1e6, 3) for v in times]
        finally:
            endpoint.cleanup(containers, pods)


def benchmark(args, endpoints, output):
    image_ids = []
    for endpoint in endpoints:
        reply = call(endpoint.images.ImageStatus, cri.ImageStatusRequest(image=cri.ImageSpec(image=args.image)))
        if not reply.image.id:
            raise ValueError("preload the pinned image on both endpoints")
        image_ids.append(reply.image.id)
    if len(set(image_ids)) != 1:
        raise ValueError("image config IDs differ")
    print(json.dumps({"runtimes": [[e.version.runtime_name, e.version.runtime_version] for e in endpoints],
                      "image_config": image_ids[0]}), flush=True)
    writer = csv.writer(output)
    writer.writerow(["runtime", "phase", "parallel", "pair", "slot", *METRICS, "wave_ms"])
    run = uuid.uuid4().hex
    results = [[] for _ in endpoints]
    names = [e.version.runtime_name for e in endpoints]
    if len(endpoints) == 3:
        names = ["after", "containerd", "before"]
    orders = list(permutations(range(len(endpoints))))
    try:
        with ThreadPoolExecutor(max_workers=args.parallel) as workers:
            for pair in range(args.warmup + args.pairs):
                warm = pair < args.warmup
                for index in orders[pair % len(orders)]:
                    endpoint = endpoints[index]
                    wave_start = time.perf_counter_ns()
                    jobs = [workers.submit(sample, endpoint, args.image, run, args.workdir)
                            for _ in range(args.parallel)]
                    completed = [job.result() for job in jobs]
                    wave_ms = round((time.perf_counter_ns() - wave_start) / 1e6, 3)
                    for slot, values in enumerate(completed):
                        writer.writerow([names[index], "warmup" if warm else "measure",
                                         args.parallel, pair, slot, *values, wave_ms])
                        output.flush()
                        if not warm:
                            results[index].append(values)
    finally:
        cleanup_errors = []
        for endpoint in endpoints:
            try:
                endpoint.cleanup(*endpoint.objects(run))
                if any(endpoint.objects(run)):
                    raise RuntimeError("benchmark objects remain on endpoint")
            except (grpc.RpcError, RuntimeError) as exc:
                cleanup_errors.append(f"{endpoint.version.runtime_name}: {exc}")
        if cleanup_errors:
            raise RuntimeError("\n".join(cleanup_errors))
    for name, samples in zip(names, results):
        summary = {}
        for key, column in zip(METRICS, zip(*samples)):
            ordered = sorted(column)
            summary[key] = [ordered[math.ceil(len(ordered)*p)-1] for p in (0.5, 0.95)]
        print(name, json.dumps(summary), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--nautilus", type=Path, required=True)
    parser.add_argument("--baseline", type=Path, help="optional second Nautilus build")
    parser.add_argument("--containerd", type=Path, required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument("--workdir", type=Path, required=True, help="absolute shared host directory")
    parser.add_argument("--output", type=Path, required=True, help="new CSV path")
    parser.add_argument("--pairs", type=int, default=30)
    parser.add_argument("--warmup", type=int, default=3)
    parser.add_argument("--parallel", type=int, default=1, help="simultaneous lifecycles per runtime")
    args = parser.parse_args()
    sockets = [args.nautilus, args.containerd] + ([args.baseline] if args.baseline else [])
    if not args.workdir.is_absolute() or len({p.resolve() for p in sockets}) != len(sockets):
        parser.error("use separate sockets and an absolute workdir")
    if "@sha256:" not in args.image or min(args.pairs, args.parallel) < 1 or args.warmup < 0:
        parser.error("use a pinned digest, positive pairs and parallelism, and nonnegative warmup")
    endpoints = [Endpoint(path) for path in sockets]
    try:
        names = [e.version.runtime_name.lower() for e in endpoints]
        if "nautilus" not in names[0] or names[1] != "containerd" or any("nautilus" not in n for n in names[2:]):
            parser.error("unexpected runtime names: " + str(names))
        with args.output.open("x", newline="") as out:
            benchmark(args, endpoints, out)
    finally:
        for endpoint in endpoints:
            endpoint.channel.close()


if __name__ == "__main__":
    main()
