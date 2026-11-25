#!/usr/bin/env python3
"""Validate cAdvisor filesystem, process, descriptor and socket series on the real CRI socket."""

import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import time
import uuid


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

with tempfile.TemporaryDirectory(prefix="nt-cgroup-") as tmp:
    tmp = Path(tmp)
    uid = str(uuid.uuid4())
    pod_spec = tmp / "pod.json"
    pod_spec.write_text(json.dumps({
        "metadata": {"name": "native-cadvisor-metrics", "namespace": "default", "uid": uid},
        "linux": {"cgroup_parent": args.cgroup_parent},
    }))
    pod = ""
    containers = []
    try:
        pod = cri("runp", "--runtime", "runc", str(pod_spec))
        workloads = {
            "busy": ("dd if=/dev/zero of=/cadvisor-writable-load bs=1048576 count=16 2>/dev/null "
                     "&& sync && exec httpd -f -p 25050"),
            "idle": "exec sleep 120",
        }
        for name, cmd in workloads.items():
            container_spec = tmp / f"{name}.json"
            container_spec.write_text(json.dumps({
                "metadata": {"name": name},
                "image": {"image": args.image},
                "command": ["/bin/sh", "-c", cmd],
                "linux": {"resources": {"cpu_period": 100000, "cpu_quota": 20000,
                                       "cpu_shares": 1024, "memory_limit_in_bytes": 134217728}},
            }))
            container = cri("create", pod, str(container_spec), str(pod_spec))
            containers.append(container)
            cri("start", container)


        for _ in range(60):
            reply = json.loads(cri("metricsp", "-o", "json"))
            pod_metrics = next((p for p in reply.get("podMetrics", []) if p.get("podSandboxId") == pod), {})
            measured = {entry["containerId"]: {sample["name"]: int(sample["value"].get("value", 0))
                                               for sample in entry["metrics"]}
                        for entry in pod_metrics.get("containerMetrics", [])}
            busy = measured.get(containers[0], {})
            idle = measured.get(containers[1], {})
            if busy.get("container_sockets", 0) >= 1 and idle:
                break
            time.sleep(0.2)
        else:
            raise AssertionError((busy, idle, pod_metrics))

        assert busy.get("container_fs_usage_bytes", 0) - idle.get("container_fs_usage_bytes", 0) >= 16 << 20, (busy, idle)
        assert busy["container_fs_limit_bytes"] >= busy["container_fs_usage_bytes"]
        assert busy["container_fs_inodes_total"] >= busy["container_fs_inodes_free"] > 0
        assert busy["container_processes"] >= 1
        assert idle["container_processes"] >= 1
        assert busy["container_file_descriptors"] >= 4
        assert idle["container_sockets"] == 0
        net = {metric["name"] for metric in pod_metrics["metrics"]}
        assert {"container_network_receive_bytes_total", "container_network_transmit_bytes_total"} <= net
        print(f"PASS CRI metricsp: fs +{busy['container_fs_usage_bytes'] - idle['container_fs_usage_bytes']} bytes, "
              f"processes {busy['container_processes']}, FDs {busy['container_file_descriptors']}, "
              f"sockets {busy['container_sockets']}; real Pod interfaces sampled")
    finally:
        for container in containers:
            # cgroupfs can report a transient busy scope after SIGKILL.
            for attempt in range(5):
                try:
                    cri("rm", "-f", container)
                    break
                except subprocess.CalledProcessError:
                    if attempt == 4:
                        raise
                    time.sleep(1)
        if pod:
            cri("stopp", pod)
            cri("rmp", pod)
