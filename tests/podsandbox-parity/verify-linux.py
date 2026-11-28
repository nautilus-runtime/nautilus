#!/usr/bin/env python3
"""Run the shared parity fixtures on an isolated local Nautilus CRI socket."""

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path


FIXTURES = Path(__file__).resolve().parent
LABEL = "io.nautilus.test/podsandbox-parity"
NAMESPACES = {"network": 0, "pid": 1, "ipc": 0}
TIMEOUT_SECONDS = 8


class Cri:
    def __init__(self, crictl, endpoint, run_id):
        self.base = [
            crictl,
            "--runtime-endpoint",
            endpoint,
            "--image-endpoint",
            endpoint,
            "--timeout=70s",
        ]
        self.run_id = run_id

    def call(self, *args, timeout=90, check=True):
        result = subprocess.run([*self.base, *args], capture_output=True, timeout=timeout)
        if check and result.returncode:
            error = result.stderr.decode(errors="replace").strip()
            raise RuntimeError(f"crictl {args[0]} failed ({result.returncode}): {error}")
        return result

    def resource_ids(self, resource):
        selector = f"{LABEL}={self.run_id}"
        if resource == "pods":
            result = self.call("pods", "--label", selector, "-o", "json")
            key = "items"
        else:
            result = self.call("ps", "-a", "--label", selector, "-o", "json")
            key = "containers"
        objects = json.loads(result.stdout).get(key) or []
        if any(obj.get("labels", {}).get(LABEL) != self.run_id for obj in objects):
            raise RuntimeError(f"crictl returned {resource} outside the run label")
        return [obj["id"] for obj in objects]

    def cleanup(self):
        errors = []
        for resource, operations in (
            ("containers", (("stop", "--timeout=0"), ("rm",))),
            ("pods", (("stopp",), ("rmp",))),
        ):
            try:
                ids = self.resource_ids(resource)
            except (RuntimeError, LookupError, OSError, ValueError, subprocess.SubprocessError) as error:
                errors.append(str(error))
                continue
            for object_id in ids:
                for operation in operations:
                    try:
                        self.call(*operation, object_id)
                    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
                        errors.append(str(error))
        remaining = {
            resource: self.resource_ids(resource) for resource in ("pods", "containers")
        }
        if any(remaining.values()) or errors:
            raise RuntimeError(f"CRI cleanup errors={errors}, remaining={remaining}")


class Case:
    def __init__(self, cri, root, name, input_bytes):
        self.cri = cri
        self.path = root / name
        self.path.mkdir()
        self.workspace = self.path / "workspace"
        self.workspace.mkdir()
        self.input_dir = self.path / "input"
        self.input_dir.mkdir()
        (self.input_dir / "file").write_bytes(input_bytes)
        self.pod_path = self.path / "pod.json"
        self.pod_path.write_text(
            json.dumps(
                {
                    "metadata": {
                        "name": name,
                        "namespace": cri.run_id,
                        "uid": str(uuid.uuid4()),
                    },
                    "labels": {LABEL: cri.run_id},
                    "log_directory": str(self.path),
                    "linux": {
                        "security_context": {"namespace_options": NAMESPACES},
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
        self.fixture = json.loads((FIXTURES / f"{name}.json").read_text())
        self.foreground = self.fixture["foreground"]
        self.members = {}

    def create(self, member, require_seccomp=False):
        name = member["id"]
        security = {"namespace_options": NAMESPACES, "no_new_privs": True}
        if require_seccomp:
            security["seccomp"] = {"profile_type": 0}
        config = {
            "metadata": {"name": name},
            "labels": {LABEL: self.cri.run_id},
            "image": {"image": member["image"]},
            "command": member.get("arguments", []),
            "working_dir": member.get("workingDirectory", ""),
            "envs": [],
            "mounts": [
                {"host_path": str(self.workspace), "container_path": "/workspace"},
                {"host_path": str(self.input_dir), "container_path": "/input", "readonly": True},
            ],
            "log_path": f"{name}.log",
            "linux": {"security_context": security},
        }
        for variable in member.get("environment", []):
            key, value = variable.split("=", 1)
            # crictl accepts text here and sends it directly as protobuf wire bytes.
            config["envs"].append({"key": key, "value": value})
        config_path = self.path / f"{name}.json"
        config_path.write_text(json.dumps(config))
        return self.cri.call(
            "create", self.pod, str(config_path), str(self.pod_path), check=not require_seccomp
        )

    def start(self):
        self.pod = self.cri.call("runp", "--runtime=vm", str(self.pod_path)).stdout.decode().strip()
        for member in self.fixture["containers"]:
            container = self.create(member).stdout.decode().strip()
            self.members[member["id"]] = container
            self.cri.call("start", container)
        self.started_at = time.monotonic()

    def status(self, member):
        result = self.cri.call("inspect", self.members[member], timeout=12)
        return json.loads(result.stdout)["status"]

    def log_diagnostics(self):
        details = []
        for member in self.members:
            path = self.path / f"{member}.log"
            try:
                with path.open("rb") as raw:
                    size = raw.seek(0, 2)
                    raw.seek(max(size - 4096, 0))
                    details.append(f"{member} CRI log tail={raw.read(4096)!r}")
            except OSError as error:
                details.append(f"{member} CRI log unavailable: {error}")
        return "; ".join(details)

    def wait_for_exit(self, expected):
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            status = self.status(self.foreground)
            if status["state"] == "CONTAINER_EXITED":
                if status["exitCode"] != expected:
                    raise RuntimeError(
                        f"{self.path.name} exited {status['exitCode']}, expected {expected}: "
                        f"reason={status.get('reason')!r}, message={status.get('message')!r}; "
                        f"{self.log_diagnostics()}"
                    )
                return
            time.sleep(0.15)
        raise TimeoutError(f"{self.path.name} did not exit; {self.log_diagnostics()}")

    def exec(self, member, *args, timeout=12):
        return self.cri.call("exec", "--sync", self.members[member], *args, timeout=timeout).stdout

    def boot_id(self):
        return uuid.UUID(self.exec("db", "cat", "/proc/sys/kernel/random/boot_id").decode().strip())

    def logs(self):
        return self.cri.call("logs", "--stream=stdout", self.members[self.foreground], timeout=12).stdout

    def check_export(self, expected):
        output = self.logs()
        if output != expected:
            raise RuntimeError(f"{self.path.name} exported {output!r}, expected {expected!r}")
        return hashlib.sha256(output).hexdigest()

    def verify_restricted_network(self):
        evidence = self.exec(
            self.foreground,
            "/bin/sh",
            "-ec",
            """busybox ip -o link show || busybox ip link show
routes=$(busybox ip route show)
if printf '%s\n' "$routes" | busybox grep -q '^default '; then
    printf 'Unexpected guest default route: %s\n' "$routes" >&2
    exit 1
fi
if route=$(busybox ip route get 198.51.100.1 2>&1); then
    printf 'Unexpected route to TEST-NET-2: %s\n' "$route" >&2
    exit 1
fi
if error=$(busybox nc -w 1 198.51.100.1 443 </dev/null 2>&1); then
    printf 'Unexpected connection to TEST-NET-2\n' >&2
    exit 1
fi
printf 'Route lookup: %s\nConnection: %s\n' "$route" "$error"
""",
            timeout=5,
        )
        print(
            "PASS network: no guest IPv4 default route, TEST-NET-2 unreachable\n"
            f"{evidence.decode().rstrip()}"
        )

    def wait_for_heartbeat(self):
        deadline = min(self.started_at + TIMEOUT_SECONDS, time.monotonic() + 4)
        heartbeat = self.workspace / "timeout.heartbeat"
        while time.monotonic() < deadline:
            try:
                if heartbeat.stat().st_size >= 2:
                    return heartbeat
            except FileNotFoundError:
                pass
            time.sleep(0.08)
        raise TimeoutError("The detached child did not produce two heartbeat writes before cancellation")

    def stop_on_deadline(self):
        deadline = self.started_at + TIMEOUT_SECONDS
        if time.monotonic() >= deadline:
            raise TimeoutError("The timeout workload's readiness checks overran its deadline")
        while time.monotonic() < deadline:
            status = self.status(self.foreground)
            if status["state"] != "CONTAINER_RUNNING":
                raise RuntimeError(
                    "Timeout workload stopped before its deadline: "
                    f"exit={status.get('exitCode')!r}, reason={status.get('reason')!r}; "
                    f"{self.log_diagnostics()}"
                )
            time.sleep(0.15)
        # CRI StopContainer takes a shutdown grace period, not a workload deadline.
        self.cri.call("stop", "--timeout=0", self.members[self.foreground])


def verify(cri, root):
    input_bytes = (FIXTURES / "input.txt").read_bytes()
    success = Case(cri, root, "success", input_bytes)
    timeout = Case(cri, root, "timeout", input_bytes)
    images = {member["image"] for case in (success, timeout) for member in case.fixture["containers"]}
    for image in sorted(images):
        cri.call("pull", image, timeout=180)

    success.start()
    success.wait_for_exit(23)
    success_digest = success.check_export(input_bytes)
    success_boot = success.boot_id()
    success.exec("db", "test", "-s", "/workspace/db.boot")
    print(
        "PASS success: exit 23, read-only input, group loopback and shared workspace, "
        f"CRI stdout SHA-256 {success_digest}"
    )

    timeout.start()
    timeout.verify_restricted_network()
    heartbeat = timeout.wait_for_heartbeat()
    timeout.stop_on_deadline()
    timeout.wait_for_exit(137)
    heartbeat_size = heartbeat.stat().st_size
    timeout_digest = timeout.check_export(b"timeout-ready\n")
    if timeout.boot_id() == success_boot or timeout.pod == success.pod:
        raise RuntimeError("The two live groups shared a sandbox or guest boot ID")
    timeout.exec("db", "test", "-s", "/workspace/timeout.started")
    timeout.exec("db", "test", "!", "-e", "/workspace/db.boot")
    success.exec("db", "test", "!", "-e", "/workspace/timeout.started")
    pong = timeout.exec("db", "/usr/local/bin/redis-cli", "-h", "127.0.0.1", "--raw", "PING")
    if pong.strip() != b"PONG":
        raise RuntimeError(f"The timeout group did not get PONG from its Redis sibling: {pong!r}")
    if success.boot_id() != success_boot:
        raise RuntimeError("The timeout affected the other sandbox")
    time.sleep(2.2)
    if heartbeat.stat().st_size != heartbeat_size:
        raise RuntimeError("Detached child heartbeat continued after CRI cancellation")
    print(
        "PASS timeout: 8-second supervisor deadline, CRI exit 137, detached child heartbeat stopped, "
        f"both groups' Redis siblings still live, CRI stdout SHA-256 {timeout_digest}"
    )

    # The workloads exercise their runtime's existing policy. An explicit request must be rejected rather than dropped.
    seccomp_probe = dict(success.fixture["containers"][-1], id="seccomp-probe")
    requested = success.create(seccomp_probe, require_seccomp=True)
    if requested.returncode == 0 or b"security profile" not in requested.stderr.lower():
        raise RuntimeError(f"Expected explicit RuntimeDefault seccomp rejection: {requested.stderr!r}")
    fields = success.exec("db", "grep", "-E", "^(Seccomp|Seccomp_filters|NoNewPrivs):", "/proc/1/status")
    policy = dict(line.split(":", 1) for line in fields.decode().splitlines() if ":" in line)
    if policy.get("NoNewPrivs", "").strip() != "1" or "Seccomp" not in policy:
        raise RuntimeError(f"Missing guest security evidence: {policy}")
    print(
        "POLICY DIFFERENCE: Linux VM rejects explicit RuntimeDefault seccomp; "
        f"guest Seccomp={policy['Seccomp'].strip()}, "
        f"filters={policy.get('Seccomp_filters', 'unreported').strip()}, NoNewPrivs=1"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime-endpoint", required=True, help="isolated local Unix CRI socket")
    parser.add_argument("--crictl", default="crictl", help="path to crictl v1.37")
    args = parser.parse_args()
    socket = Path(args.runtime_endpoint.removeprefix("unix://"))
    if not socket.is_absolute() or not socket.is_socket():
        parser.error("--runtime-endpoint must identify an existing local Unix socket")
    run_id = f"podsandbox-parity-{uuid.uuid4().hex}"
    cri = Cri(args.crictl, f"unix://{socket}", run_id)
    root = Path(tempfile.mkdtemp(prefix=f"{run_id}-"))
    print(f"Scoped CRI resources: {LABEL}={run_id}, host test directory: {root}", flush=True)
    try:
        verify(cri, root)
    finally:
        try:
            cri.cleanup()
        except (RuntimeError, LookupError, OSError, ValueError, subprocess.SubprocessError) as error:
            raise RuntimeError(f"{error}; preserved guest mount sources at {root}") from error
        shutil.rmtree(root)
        print("PASS cleanup: no labeled containers or Pod sandboxes remain")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, LookupError, OSError, ValueError, subprocess.SubprocessError) as error:
        sys.exit(f"Linux parity verification failed: {error}")
