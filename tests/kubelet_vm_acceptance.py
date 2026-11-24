#!/usr/bin/env python3
"""Run on a disposable Kubernetes node with Nautilus, bridge/host-local CNI and nested or native KVM."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import selectors
import signal
import struct
import subprocess
import tempfile
import termios
import time
import tomllib
import urllib.request
import uuid


def run(argv, **kwargs):
    return subprocess.check_output(argv, text=True, timeout=45, **kwargs).strip()


def wait(check, seconds=60):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if check():
            return
        time.sleep(1)
    raise TimeoutError(f"condition did not become true in {seconds}s")


def attached_init(kubectl):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    process = subprocess.Popen([*kubectl, "attach", "-it", "alpha", "-c", "console"],
                               stdin=slave, stdout=slave, stderr=slave)
    os.close(slave)
    try:
        output = b""
        with selectors.DefaultSelector() as ready:
            ready.register(master, selectors.EVENT_READ)
            end = time.monotonic() + 20
            while b"init:1:ntk-attach" not in output:
                if time.monotonic() >= end or process.poll() is not None:
                    raise RuntimeError(output.decode(errors="replace"))
                os.write(master, b"ntk-attach\n")
                if ready.select(0.5):
                    output += os.read(master, 4096)
    finally:
        process.terminate()
        process.wait(timeout=5)
        os.close(master)


def forwarded_http(kubectl):
    process = subprocess.Popen([*kubectl, "port-forward", "pod/alpha", ":8080"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        with selectors.DefaultSelector() as ready:
            ready.register(process.stdout, selectors.EVENT_READ)
            assert ready.select(20), "kubectl never published its local forwarding port"
            line = process.stdout.readline()
            match = re.search(rb"127\.0\.0\.1:(\d+)", line)
            assert match, line
        with urllib.request.urlopen("http://127.0.0.1:" + match[1].decode() + "/host.txt", timeout=10) as response:
            assert response.read().strip() == b"host-visible"
    finally:
        process.terminate()
        process.wait(timeout=5)


def workload(name, command, image, **extras):
    return dict(name=name, image=image, imagePullPolicy="IfNotPresent",
                command=["/bin/sh", "-c", command],
                resources={"limits": {"memory": "192Mi", "cpu": "500m"}}, **extras)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--node", required=True)
    parser.add_argument("--daemon-unit", required=True, help="Nautilus unit on this disposable node (the test SIGKILLs its main PID)")
    parser.add_argument("--config", required=True, type=Path, help="the running Nautilus TOML")
    parser.add_argument("--ipam-dir", type=Path, required=True, help="host-local dataDir/CNI network name")
    parser.add_argument("--image", default="registry.k8s.io/e2e-test-images/busybox:1.29-2",
                        help="busybox image with sh, wget and httpd")
    parser.add_argument("--kubectl", default="kubectl", help="uses KUBECONFIG")
    parser.add_argument("--crictl", default="crictl")
    args = parser.parse_args()
    cfg = tomllib.loads(args.config.read_text())
    ep = cfg["cri"]["listen_address"]
    cri = [args.crictl, "--runtime-endpoint", ep, "--image-endpoint", ep]
    namespace = "nt-vm-" + uuid.uuid4().hex[:10]
    k = [args.kubectl, "-n", namespace]

    def kube(*argv):
        return run([*k, *argv])

    def exec_in(pod, container, *argv):
        return kube("exec", pod, "-c", container, "--", *argv)

    def apply(obj):
        kube_input = json.dumps(obj)
        return run([*k, "apply", "-f", "-"], input=kube_input)

    def pod(name, containers, volumes):
        return dict(apiVersion="v1", kind="Pod", metadata={"name": name},
                    spec=dict(runtimeClassName=namespace, containers=containers,
                              terminationGracePeriodSeconds=1, volumes=volumes, restartPolicy="Never",
                              automountServiceAccountToken=False))

    def snapshot():
        facts = []
        for name, container in [("alpha", "web"), ("beta", "client")]:
            pod_data = json.loads(kube("get", "pod", name, "-o", "json"))
            assert pod_data["spec"]["nodeName"] == args.node
            status = pod_data["status"]
            ids = [c["containerID"] for c in status["containerStatuses"] if c["ready"]]
            assert len(ids) == (2 if name == "alpha" else 1)
            boot = exec_in(name, container, "cat", "/proc/sys/kernel/random/boot_id")
            uuid.UUID(boot)
            facts.append((status["podIP"], ids, boot))
        assert facts[0][0] != facts[1][0] and facts[0][2] != facts[1][2]
        assert exec_in("alpha", "console", "cat", "/proc/sys/kernel/random/boot_id") == facts[0][2]
        return facts

    def exercise(facts, projected):
        assert "console-ready" in kube("logs", "alpha", "-c", "console")
        assert "web-ready" in kube("logs", "alpha", "-c", "web")
        assert "client-ready" in kube("logs", "beta", "-c", "client")
        assert exec_in("beta", "client", "wget", "-qO-", "-T", "5",
                       "http://" + facts[0][0] + ":8080/host.txt") == "host-visible"
        assert exec_in("alpha", "web", "cat", "/settings/mode") == projected
        assert subprocess.run([*k, "exec", "beta", "-c", "client", "--", "sh", "-c", "exit 35"]).returncode == 35
        attached_init(k)
        forwarded_http(k)

    # Get node registration from the real API before creating a RuntimeClass.
    def advertised():
        node = json.loads(kube("get", "node", args.node, "-o", "json"))
        names = {h["name"] for h in node["status"].get("runtimeHandlers", [])}
        return {"", "runc", "vm"}.issubset(names)
    wait(advertised)
    kube("create", "namespace", namespace)
    records = []
    try:
        node = json.loads(kube("get", "node", args.node, "-o", "json"))
        hostname = node["metadata"]["labels"]["kubernetes.io/hostname"]
        apply(dict(apiVersion="node.k8s.io/v1", kind="RuntimeClass", metadata={"name": namespace},
                   handler="vm", scheduling={"nodeSelector": {"kubernetes.io/hostname": hostname}}, overhead={"podFixed": {"memory": "128Mi"}}))
        with tempfile.TemporaryDirectory(prefix="nautilus-kubelet-") as volume:
            Path(volume, "host.txt").write_text("host-visible\n")
            apply(dict(apiVersion="v1", kind="ConfigMap", metadata={"name": "settings"}, data={"mode": "projected"}))
            console = workload("console", 'echo console-ready; while :; do if read -r s; then echo "init:$$$$:$s"; else sleep 1; fi; done',
                               args.image, stdin=True, tty=True)
            web = workload("web", "echo web-ready; exec httpd -f -p 8080 -h /public", args.image,
                           volumeMounts=[dict(name="host", mountPath="/public"),
                                         dict(name="settings", mountPath="/settings", readOnly=True)])
            client = workload("client", "echo client-ready; exec sleep 3600", args.image)
            client["resources"]["limits"] = {"cpu": "1", "memory": "384Mi"}
            apply(pod("alpha", [console, web], [dict(name="host", hostPath=dict(path=volume, type="Directory")),
                                                 dict(name="settings", configMap={"name": "settings"})]))
            apply(pod("beta", [client], []))
            subprocess.run([*k, "wait", "--for=condition=Ready", "--timeout=120s", "pod/alpha", "pod/beta"], check=True)
            before = snapshot()
            exercise(before, "projected")
            exec_in("alpha", "web", "sh", "-c", "echo guest-wrote >/public/guest.txt")
            assert Path(volume, "guest.txt").read_text().strip() == "guest-wrote"
            assert subprocess.run([*k, "exec", "alpha", "-c", "web", "--", "sh", "-c",
                                   "echo invalid >/settings/mode"], capture_output=True).returncode != 0
            kube("patch", "configmap", "settings", "--type=merge", "-p", '{"data":{"mode":"updated"}}')
            wait(lambda: exec_in("alpha", "web", "cat", "/settings/mode") == "updated", 180)
            for name in ("alpha", "beta"):
                pod_id = run([*cri, "pods", "--name", "^" + name + "$", "--namespace", "^" + namespace + "$", "-q"])
                uuid.UUID(pod_id)
                records.append(pod_id)
                units = run(["systemctl", "list-units", "--state=running", "--no-legend", "--plain",
                             f"nautilus-vm-{pod_id}-*.service"]).split()
                assert units, f"VMM unit for {pod_id} is missing"
                unit = units[0]
                group = run(["systemctl", "show", unit, "-p", "ControlGroup", "--value"])
                cgroup = Path("/sys/fs/cgroup", group.lstrip("/"))
                assert int((cgroup / "memory.max").read_text()) == 512 << 20
                assert (cgroup / "cpu.max").read_text().strip() == "100000 100000"
                assert (cgroup / "memory.oom.group").read_text().strip() == "1"
            for name, container in [("alpha", "web"), ("beta", "client")]:
                assert exec_in(name, container, "grep", "-c", "^processor", "/proc/cpuinfo") == "1"
                mem = exec_in(name, container, "grep", "^MemTotal:", "/proc/meminfo")
                assert 256 * 1024 < int(mem.split()[1]) <= 384 * 1024, mem
                assert exec_in(name, container, "cat", "/proc/1/oom_score_adj") == "-997"
            guest_ids = {raw.split("://", 1)[1] for _, ids, _ in before for raw in ids}
            container_stats = json.loads(run([*cri, "stats", "-o", "json"])).get("stats", [])
            pod_stats = json.loads(run([*cri, "statsp", "-o", "json"])).get("stats", [])
            assert guest_ids.isdisjoint(s["attributes"]["id"] for s in container_stats)
            assert set(records).isdisjoint(s["attributes"]["id"] for s in pod_stats)
            assert all((args.ipam_dir / ip).is_file() for ip, _, _ in before)
            pid = int(run(["systemctl", "show", args.daemon_unit, "-p", "MainPID", "--value"]))
            assert pid > 1 and Path(f"/proc/{pid}/exe").resolve().name.startswith("nautilusd")
            os.kill(pid, signal.SIGKILL)
            run(["systemctl", "restart", args.daemon_unit])
            wait(lambda: subprocess.run([*cri, "info"], capture_output=True).returncode == 0)
            assert snapshot() == before
            exercise(before, "updated")
            print("Guest IPs, container IDs and boots after daemon SIGKILL:", before)
            kube("delete", "pod", "alpha", "beta", "--grace-period=1", "--wait=false")
            state = Path(cfg["runtime"]["state_dir"])
            def cleaned():
                for id_ in records:
                    if any(p.exists() for p in [state/"vms"/id_, state/"vms"/(id_+".json"),
                                               state/"vm-network"/(id_+".json"), Path(cfg["network"]["netns_dir"])/("vm-"+id_)]):
                        return False
                    if run(["systemctl", "list-units", "--state=running", "--no-legend", "--plain", f"nautilus-*-{id_}-*.service"]):
                        return False
                return all(not (args.ipam_dir / ip).exists() for ip, _, _ in before)
            wait(cleaned, 120)
            print("Released VMM/virtiofsd units, owner journals and network namespaces:", records)
    finally:
        kube("delete", "namespace", namespace, "--wait=false", "--ignore-not-found")
        kube("delete", "runtimeclass", namespace, "--ignore-not-found")


if __name__ == "__main__":
    main()
