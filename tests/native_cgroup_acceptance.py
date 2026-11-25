#!/usr/bin/env python3
"""Compare CRI workload accounting with an isolated daemon's Linux cgroup v2 files."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import uuid


def counter(stats, group, name):
    return int(stats[group][name]["value"])


def keyed_counter(path, key):
    return int(next(line.split()[1] for line in path.read_text().splitlines()
                    if line.split()[0] == key))


def init_cpu_ticks(pid):
    ticks = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return int(ticks[11]) + int(ticks[12])


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--socket", required=True)
parser.add_argument("--runc-root", required=True, type=Path)
parser.add_argument("--crictl", default="crictl")
parser.add_argument("--image", default="docker.io/library/busybox:latest",
                    help="image already pulled into the daemon")
parser.add_argument("--cgroup-parent", default="", help="optional CRI Linux cgroup parent")
args = parser.parse_args()
endpoint = args.socket if args.socket.startswith("unix://") else f"unix://{args.socket}"
command = [args.crictl, "--runtime-endpoint", endpoint, "--image-endpoint", endpoint]


def cri(*argv):
    return subprocess.check_output([*command, *argv], text=True, timeout=60).strip()


def container_stats(container):
    stats = json.loads(cri("stats", "--id", container, "-o", "json"))["stats"]
    assert len(stats) == 1 and stats[0]["attributes"]["id"] == container, stats
    return stats[0]


with tempfile.TemporaryDirectory(prefix="nt-cgroup-") as tmp:
    tmp = Path(tmp)
    uid = str(uuid.uuid4())
    pod_spec = tmp / "pod.json"
    pod_spec.write_text(json.dumps({
        "metadata": {"name": "native-cgroup-stats", "namespace": "default", "uid": uid},
        "linux": {"cgroup_parent": args.cgroup_parent},
    }))
    pod = ""
    containers = []
    groups = set()
    try:
        pod = cri("runp", "--runtime", "runc", str(pod_spec))
        workloads = {
            "busy": ("dd if=/dev/zero of=/dev/shm/stats-load bs=1048576 count=16 2>/dev/null "
                     "|| exit; sh -c 'while :; do :; done' & wait"),
            "idle": "exec sleep 120",
        }
        for name, cmd in workloads.items():
            container_spec = tmp / f"{name}.json"
            container_spec.write_text(json.dumps({
                "metadata": {"name": name},
                "image": {"image": args.image},
                "command": ["/bin/sh", "-c", cmd],
                "linux": {},
            }))
            container = cri("create", pod, str(container_spec), str(pod_spec))
            containers.append(container)
            cri("start", container)

        def direct_sample(container):
            state = json.loads((args.runc_root / container / "state.json").read_text())
            pid = state["pid"]
            lines = Path(f"/proc/{pid}/cgroup").read_text().splitlines()
            member = next(line.removeprefix("0::/") for line in lines if line.startswith("0::/"))
            group = Path("/sys/fs/cgroup") / member
            groups.add(group)
            cpu = lambda: keyed_counter(group / "cpu.stat", "usage_usec") * 1000
            before = cpu()
            sample = container_stats(container)
            after = cpu()
            usage = counter(sample, "memory", "usageBytes")
            working_set = counter(sample, "memory", "workingSetBytes")
            current = int((group / "memory.current").read_text())
            expected = max(0, current - keyed_counter(group / "memory.stat", "inactive_file"))
            measured = counter(sample, "cpu", "usageCoreNanoSeconds")
            assert before <= measured <= after, (before, measured, after)
            # Kernel counters can change while the process is running.
            assert abs(working_set - expected) < 1 << 20, (working_set, expected)
            assert 0 <= working_set <= usage, (working_set, usage)
            return (measured, working_set, pid, group)

        busy, idle = containers
        busy_first = direct_sample(busy)
        idle_first = direct_sample(idle)
        init_start = init_cpu_ticks(busy_first[2])
        time.sleep(1.5)
        busy_last = direct_sample(busy)
        idle_last = direct_sample(idle)
        init_consumed = (init_cpu_ticks(busy_last[2]) - init_start) * 1_000_000_000 // os.sysconf("SC_CLK_TCK")
        busy_ns = busy_last[0] - busy_first[0]
        idle_ns = idle_last[0] - idle_first[0]
        assert busy_ns > 100_000_000, (busy_ns, idle_ns)
        assert busy_ns - max(init_consumed, idle_ns) > 100_000_000, (busy_ns, init_consumed, idle_ns)
        assert busy_last[1] - idle_last[1] >= 15 << 20, (busy_last, idle_last)
        assert busy_last[3] != idle_last[3], (busy_last, idle_last)

        samples = json.loads(cri("statsp", "-o", "json"))["stats"]
        stats = next(s["linux"] for s in samples if s["attributes"]["id"] == pod)
        reported = stats["containers"]
        assert {sample["attributes"]["id"] for sample in reported} == set(containers)
        for group, field in [("cpu", "usageCoreNanoSeconds"), ("memory", "workingSetBytes")]:
            assert counter(stats, group, field) == sum(counter(s, group, field) for s in reported)
        print(f"PASS native cgroup v2: busy CPU +{busy_ns} ns, init +{init_consumed} ns, "
              f"idle +{idle_ns} ns; working sets {busy_last[1]} and {idle_last[1]} bytes; "
              f"Pod CPU and working set equal the container sums")
    finally:
        cleanup_error = None
        for container in containers:
            try:
                cri("rm", "-f", container)
            except subprocess.CalledProcessError as error:
                # Clean up the fixture but fail if the first removal failed.
                cleanup_error = cleanup_error or error
                time.sleep(1)
                try:
                    cri("rm", "-f", container)
                except subprocess.CalledProcessError:
                    pass
        if pod:
            for operation in ("stopp", "rmp"):
                try:
                    cri(operation, pod)
                except subprocess.CalledProcessError as error:
                    cleanup_error = cleanup_error or error
        if cleanup_error is not None:
            raise cleanup_error
        assert all(not group.exists() for group in groups), groups
