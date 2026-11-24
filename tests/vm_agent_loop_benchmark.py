#!/usr/bin/env python3
"""Measure real VM Pod lifecycles on one dedicated, preconfigured Nautilus node."""

import argparse
import hashlib
from concurrent.futures import ThreadPoolExecutor
import ipaddress
import json
import math
from pathlib import Path
import subprocess
import tempfile
import time
import tomllib
import uuid


def command(argv, timeout=90):
    result = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"{argv!r} exited {result.returncode}: {result.stderr.strip()}")
    return result.stdout.strip()


def measure(action):
    start = time.perf_counter_ns()
    value = action()
    return value, (time.perf_counter_ns() - start) / 1e6


def wait_for(check, timeout=45):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.1)
    raise TimeoutError(f"{check.__name__} did not complete in {timeout} seconds")


def distribution(values):
    ordered = sorted(values)
    return {
        key: round(ordered[math.ceil(fraction * len(ordered)) - 1], 3)
        for key, fraction in (("p50", 0.5), ("p95", 0.95))
    } | {"n": len(values)}


def snapshot_resources(cfg, ipam):
    state = Path(cfg["runtime"]["state_dir"])
    paths = set((state / "vms").glob("*")) | set((state / "vm-network").glob("*.json"))
    paths.update(Path(cfg["network"]["netns_dir"]).glob("vm-*"))
    for entry in ipam.glob("*"):
        try:
            ipaddress.ip_address(entry.name)
            paths.add(entry)
        except ValueError:
            pass
    units = command(
        [
            "systemctl",
            "list-units",
            "--all",
            "--plain",
            "--no-legend",
            "nautilus-vm-*.service",
            "nautilus-virtiofs-*.service",
        ]
    )
    return sorted([str(p) for p in paths] + [line.split()[0] for line in units.splitlines()])


def service_memory(unit):
    cgroup = command(["systemctl", "show", unit, "--property=ControlGroup", "--value"])
    if not cgroup.startswith("/") or cgroup == "/":
        raise RuntimeError(f"missing cgroup for {unit}")
    group = Path("/sys/fs/cgroup", cgroup.lstrip("/"))
    members = {int(pid) for f in group.rglob("cgroup.procs") for pid in f.read_text().split()}
    if not members:
        raise RuntimeError(f"{unit} cgroup had no live processes")
    pss = 0
    for pid in sorted(members):
        rollup = Path(f"/proc/{pid}/smaps_rollup").read_text()
        pss += next(int(s.split()[1]) * 1024 for s in rollup.splitlines() if s.startswith("Pss:"))
    return {
        "pids": sorted(members),
        "pss_bytes": pss,
        "memory_current": int((group / "memory.current").read_text()),
        "memory_max": (group / "memory.max").read_text().strip(),
        "cpu_max": (group / "cpu.max").read_text().strip(),
    }


class Session:
    def __init__(self, cri, args, namespace, root):
        self.cri, self.args = cri, args
        self.uid, self.pod, self.container = str(uuid.uuid4()), "", ""
        self.path = root / self.uid
        self.path.mkdir()
        labels = {"io.nautilus.benchmark": namespace}
        ns = {"network": 0, "pid": 1, "ipc": 0}
        self.pod_config = self.path / "pod.json"
        self.pod_config.write_text(
            json.dumps(
                {
                    "metadata": {
                        "name": "tool-" + self.uid,
                        "namespace": namespace,
                        "uid": self.uid,
                    },
                    "labels": labels,
                    "log_directory": str(self.path),
                    "linux": {
                        "security_context": {"namespace_options": ns},
                        "resources": {
                            "memory_limit_in_bytes": 384 << 20,
                            "cpu_quota": 100000,
                            "cpu_period": 100000,
                        },
                        "overhead": {"memory_limit_in_bytes": 128 << 20},
                    },
                }
            )
        )
        self.config = self.path / "container.json"
        self.config.write_text(
            json.dumps(
                {
                    "metadata": {"name": "worker"},
                    "labels": labels,
                    "image": {"image": args.image},
                    "command": ["/bin/sh", "-c", "exec sleep 86400"],
                    "log_path": "worker.log",
                    "linux": {"security_context": {"namespace_options": ns}},
                }
            )
        )

    def execute(self, argv):
        return self.cri("exec", "--sync", self.container, *argv)

    def start(self):
        result = {}
        self.started_at = time.monotonic()
        self.pod, result["sandbox_ms"] = measure(
            lambda: self.cri("runp", "--runtime=vm", str(self.pod_config))
        )
        self.container, result["create_ms"] = measure(
            lambda: self.cri("create", self.pod, str(self.config), str(self.pod_config))
        )
        _, result["start_ms"] = measure(lambda: self.cri("start", self.container))
        boot, result["first_exec_ms"] = measure(
            lambda: self.execute(["cat", "/proc/sys/kernel/random/boot_id"])
        )
        result["fresh_to_first_exec_ms"] = sum(result.values())
        status = json.loads(self.cri("inspectp", self.pod))["status"]
        if status["state"] != "SANDBOX_READY":
            raise RuntimeError(f"{self.pod} reported {status['state']}")
        self.facts = {
            **result,
            "pod": self.pod,
            "container": self.container,
            "ip": str(ipaddress.ip_address(status["network"]["ip"])),
            "boot": str(uuid.UUID(boot)),
        }
        return self.facts

    def memory(self):
        state = Path(self.args.cfg["runtime"]["state_dir"])
        owner = json.loads((state / "vms" / (self.pod + ".json")).read_text())
        assert owner["id"] == self.pod and owner["boot"] == self.facts["boot"]
        suffix = self.pod + "-" + owner["nonce"] + ".service"
        self.facts["idle"] = {
            name: service_memory("nautilus-" + name + "-" + suffix) for name in ("vm", "virtiofs")
        }
        assert self.facts["idle"]["vm"]["memory_max"] == str(512 << 20)
        assert self.facts["idle"]["vm"]["cpu_max"] == "100000 100000"

    def warm(self):
        last = "seed"
        samples = []
        for i in range(self.args.execs):
            # Carry data between real tool calls in the same container filesystem.
            cmd = [
                "sh",
                "-c",
                'set -e; if test -f /tmp/nt-tool-state; then test "$(cat /tmp/nt-tool-state)" = "$1"; fi; '
                'echo "$2" >/tmp/nt-tool-state; cat /tmp/nt-tool-state',
                "tool",
                last,
                str(i),
            ]
            out, elapsed = measure(lambda: self.execute(cmd))
            if out != str(i):
                raise RuntimeError(f"guest workspace returned {out!r}")
            samples.append(elapsed)
            last = str(i)
        self.facts["warm_exec_ms"] = samples

    def agent(self):
        from vm_agent_loop_workload import run_agent

        begin = time.monotonic()
        try:
            result = run_agent(self.execute, self.args.model_url, self.args.model)
            first = result.pop("first_tool_at")
            self.facts["fresh_to_first_model_tool_ms"] = (first - self.started_at) * 1000
            self.facts["agent"] = result
        except Exception as error:
            # Model and tool failures are observations; the remaining VMs still run.
            self.facts["agent_error"] = f"{type(error).__name__}: {error}"
        self.facts["agent_ms"] = (time.monotonic() - begin) * 1000

    def cancel(self, base):
        pidfile = "/tmp/nt-cancel-pid"
        proc = subprocess.Popen(
            [
                *base,
                "exec",
                "--sync",
                self.container,
                "sh",
                "-c",
                'sleep 240 & p=$!; echo "$p" >"$1"; wait "$p"',
                "cancel",
                pidfile,
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            wait_for(lambda: self.execute(["sh", "-c", f"test -s {pidfile} && echo ready || :"]) == "ready")
            pid = int(self.execute(["cat", pidfile]))
            assert pid > 1 and proc.poll() is None
            before = self.execute(["cat", f"/proc/{pid}/stat"]).split()
            assert before[1] == "(sleep)" and before[2] not in ("Z", "X")
            self.facts["cancel_child"] = {"pid": pid, "start_ticks": int(before[21]), "state": before[2]}
            start = time.perf_counter_ns()
            proc.terminate()
            proc.wait(timeout=8)

            def stopped_child():
                # A non-reaping workload PID 1 can retain an exited orphan as a
                # zombie until container removal. Record that state explicitly.
                fields = self.execute(["sh", "-c", f"cat /proc/{pid}/stat 2>/dev/null || echo gone"]).split()
                if fields == ["gone"]:
                    state = "gone"
                elif fields[21] != before[21] or fields[1] != before[1]:
                    state = "pid_reused"
                else:
                    state = fields[2]
                self.facts["cancel_child_state"] = state
                return state in ("Z", "gone", "pid_reused")

            wait_for(stopped_child)
            assert self.execute(["cat", "/proc/sys/kernel/random/boot_id"]) == self.facts["boot"]
            self.facts["cancel_to_verified_ms"] = (time.perf_counter_ns() - start) / 1e6
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()

    def stop(self):
        if self.container:
            self.cri("stop", "--timeout=0", self.container)
            self.cri("rm", self.container)
        if self.pod:
            self.cri("stopp", self.pod)
            self.cri("rmp", self.pod)


def run(args, report):
    cfg = args.cfg
    base = [
        args.crictl,
        "--runtime-endpoint",
        cfg["cri"]["listen_address"],
        "--image-endpoint",
        cfg["cri"]["listen_address"],
        "--timeout=70s",
    ]

    def cri(*args):
        return command([*base, *args])

    def log(record):
        print(json.dumps(record, separators=(",", ":")), file=report, flush=True)

    info = json.loads(cri("info"))
    if not any(h.get("name") == "vm" for h in info["runtimeHandlers"]):
        raise RuntimeError("the VM runtime handler has not passed its node readiness probe")
    image = json.loads(cri("inspecti", args.image))["status"]
    label = "nt-loop-" + uuid.uuid4().hex[:12]
    baseline = snapshot_resources(cfg, args.ipam_dir)
    daemon = None
    if args.daemon_unit:
        main_pid = int(command(["systemctl", "show", args.daemon_unit, "-p", "MainPID", "--value"]))
        executable = Path(f"/proc/{main_pid}/exe")
        with executable.open("rb") as binary:
            daemon = {
                "sha256": hashlib.file_digest(binary, "sha256").hexdigest(),
                "executable": str(executable.resolve()),
                "idle": service_memory(args.daemon_unit),
            }
    log(
        {
            "event": "environment",
            "run": label,
            "kernel": command(["uname", "-r"]),
            "node_cpu": command(["lscpu", "-J"]),
            "node_meminfo": Path("/proc/meminfo").read_text(),
            "load": Path("/proc/loadavg").read_text().strip(),
            "runtime": cri("version"),
            "crictl": command([args.crictl, "--version"]),
            "vmm": command([cfg["runtime"]["sandboxes"]["vm"]["vmm"], "--version"]),
            "virtiofsd": command([cfg["runtime"]["sandboxes"]["vm"]["virtiofsd"], "--version"]),
            "image": args.image,
            "config_id": image["id"],
            "resource_baseline": baseline,
            "daemon": daemon,
        }
    )
    with tempfile.TemporaryDirectory(prefix=label + "-") as temp:
        root = Path(temp)
        try:
            for level in args.levels:
                for wave in range(args.singles if level == 1 else args.waves):
                    sessions = [Session(cri, args, label, root) for _ in range(level)]
                    begin = time.perf_counter_ns()
                    try:
                        with ThreadPoolExecutor(max_workers=level) as pool:

                            def phase(fn):
                                return list(pool.map(fn, sessions))

                            with_agent = bool(args.model_url and wave < args.agent_waves)
                            start_wave = time.monotonic()

                            def launch(session):
                                facts = session.start()
                                session.ready_at = time.monotonic()
                                if with_agent:
                                    session.agent()
                                return facts

                            facts, launch_and_agent_ms = measure(lambda: phase(launch))
                            wave_start_ms = (max(s.ready_at for s in sessions) - start_wave) * 1000
                            for key in ("pod", "boot", "ip", "container"):
                                if len({f[key] for f in facts}) != level:
                                    raise RuntimeError(f"concurrent sessions shared {key}")
                            time.sleep(args.idle_seconds)
                            phase(Session.memory)
                            _, warm_ms = measure(lambda: phase(Session.warm))
                            if wave == 0:
                                sessions[-1].cancel(base)
                                for peer in sessions[:-1]:
                                    assert (
                                        peer.execute(["cat", "/proc/sys/kernel/random/boot_id"])
                                        == peer.facts["boot"]
                                    )
                                    assert peer.execute(["cat", "/tmp/nt-tool-state"]) == str(args.execs - 1)
                                sessions[-1].facts["peers_verified_after_cancel"] = len(sessions) - 1
                    except Exception as exc:
                        log(
                            {
                                "event": "failed_wave",
                                "level": level,
                                "wave": wave,
                                "error": str(exc),
                                "elapsed_ms": (time.perf_counter_ns() - begin) / 1e6,
                                "sessions": [
                                    {
                                        "pod": s.pod,
                                        "container": s.container,
                                        "completed": getattr(s, "facts", None),
                                    }
                                    for s in sessions
                                ],
                            }
                        )
                        raise
                    finally:
                        # The start/exec executor has settled all in-flight RPCs.
                        with ThreadPoolExecutor(max_workers=level) as reaper:
                            _, release_ms = measure(lambda: list(reaper.map(Session.stop, sessions)))
                    current = snapshot_resources(cfg, args.ipam_dir)
                    if current != baseline:
                        raise RuntimeError(f"residual node resources {set(current) ^ set(baseline)}")
                    log(
                        {
                            "event": "wave",
                            "level": level,
                            "wave": wave,
                            "start_wave_ms": wave_start_ms,
                            "warm_wave_ms": warm_ms,
                            "launch_and_agent_wave_ms": launch_and_agent_ms if with_agent else None,
                            "release_wave_ms": release_ms,
                            "total_wave_ms": (time.perf_counter_ns() - begin) / 1e6,
                            "sessions": facts,
                        }
                    )
                    print(
                        f"finished {level} simultaneous sessions, wave {wave + 1}",
                        flush=True,
                    )
        finally:
            # A timeout before runp/create returned may still have allocated an object.
            for verb, field, removal in [
                ("ps", "containers", ("stop", "rm")),
                ("pods", "items", ("stopp", "rmp")),
            ]:
                flags = ["-a"] if verb == "ps" else []
                records = json.loads(cri(verb, *flags, "-o", "json")).get(field, [])
                for r in records:
                    if r.get("labels", {}).get("io.nautilus.benchmark") == label:
                        for op in removal:
                            cri(op, r["id"])
            wait_for(lambda: snapshot_resources(cfg, args.ipam_dir) == baseline)
            for verb, field, flags in [
                ("ps", "containers", ["-a"]),
                ("pods", "items", []),
            ]:
                records = json.loads(cri(verb, *flags, "-o", "json")).get(field, [])
                assert not any(r.get("labels", {}).get("io.nautilus.benchmark") == label for r in records)
            log({"event": "cleanup", "remaining_cri_objects": 0, "resource_delta": []})


def summarize(records):
    waves = [r for r in records if r["event"] == "wave"]
    for level in sorted({r["level"] for r in waves}):
        group = [w for w in waves if w["level"] == level]
        sessions = [s for w in group for s in w["sessions"]]
        agents = [s for s in sessions if "agent" in s]
        failures = [s["agent_error"] for s in sessions if "agent_error" in s]
        summary = {
            "concurrency": level,
            "fresh_ms": distribution([s["fresh_to_first_exec_ms"] for s in sessions]),
            "warm_ms": distribution([v for s in sessions for v in s["warm_exec_ms"]]),
            "idle_pss_MiB": distribution(
                [sum(u["pss_bytes"] for u in s["idle"].values()) / (1 << 20) for s in sessions]
            ),
            "cleanup_wave_ms": distribution([w["release_wave_ms"] for w in group]),
            "agent_successes": len(agents),
            "agent_errors": failures,
        }
        if agents:
            summary.update(
                first_model_tool_ms=distribution([s["fresh_to_first_model_tool_ms"] for s in agents]),
                agent_ms=distribution([s["agent_ms"] for s in agents]),
                model_ms=distribution([v for s in agents for v in s["agent"]["inference_ms"]]),
                agent_tool_ms=distribution([v for s in agents for v in s["agent"]["tool_ms"]]),
            )
        print(json.dumps(summary))
    if any(r["event"] == "failed_wave" for r in records):
        raise SystemExit("failed lifecycle wave recorded in report")
    if any("agent_error" in s for w in waves for s in w["sessions"]):
        raise SystemExit("agent session failures recorded in report")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--report", type=Path, required=True, help="new JSON Lines evidence path")
    p.add_argument("--summarize", action="store_true", help="read an existing report")
    p.add_argument("--config", type=Path, help="running daemon config on the measurement node")
    p.add_argument(
        "--ipam-dir",
        type=Path,
        help="its host-local IPAM directory (dataDir/network name)",
    )
    p.add_argument("--image", help="preloaded immutable OCI reference with BusyBox/POSIX tools")
    p.add_argument("--crictl", default="crictl")
    p.add_argument("--daemon-unit", help="collect the daemon binary SHA-256 and idle service PSS")
    p.add_argument("--levels", type=int, nargs="+", default=[1, 2, 4, 8])
    p.add_argument("--singles", type=int, default=20)
    p.add_argument("--waves", type=int, default=3)
    p.add_argument("--execs", type=int, default=10)
    p.add_argument("--idle-seconds", type=float, default=1)
    p.add_argument("--agent-waves", type=int, default=1)
    p.add_argument("--model-url")
    p.add_argument("--model", default="nautilus-bench")
    args = p.parse_args()
    if not args.summarize:
        if not args.config or not args.ipam_dir or not args.image or "@sha256:" not in args.image:
            p.error("supply the active config, IPAM directory, and a preloaded immutable image reference")
        if (
            min(*args.levels, args.singles, args.waves, args.execs) < 1
            or min(args.idle_seconds, args.agent_waves) < 0
        ):
            p.error("invalid duration or sample count")
        args.cfg = tomllib.loads(args.config.read_text())
        with args.report.open("x") as report:
            run(args, report)
    summarize([json.loads(s) for s in args.report.read_text().splitlines()])


if __name__ == "__main__":
    main()
