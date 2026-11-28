#!/usr/bin/env python3

import json
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from dataclasses import dataclass
from pathlib import Path


BUSYBOX = "docker.io/library/busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec"
READY_TIMEOUT = 180


@dataclass
class Workload:
    name: str
    process: subprocess.Popen
    stdout: Path
    stderr: Path

    def start_result(self, sessions: Path, earlier_sessions: set):
        deadline = time.monotonic() + READY_TIMEOUT
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"{self.name} exited {self.process.returncode} before readiness")
            for line in self.stdout.read_text(errors="replace").splitlines():
                if not line.startswith("READY "):
                    continue
                boot_id = uuid.UUID(line.removeprefix("READY "))
                new_sessions = session_directories(sessions) - earlier_sessions
                if len(new_sessions) != 1:
                    raise RuntimeError(f"{self.name} created {len(new_sessions)} session directories")
                session = new_sessions.pop()
                for message in self.stderr.read_text(errors="replace").splitlines():
                    try:
                        status = json.loads(message)
                    except json.JSONDecodeError:
                        continue
                    if isinstance(status, dict) and "id" in status and "containers" in status:
                        sandbox = session / f"sandbox-{uuid.UUID(status['id'])}"
                        if not sandbox.is_dir():
                            raise RuntimeError(f"{self.name} sandbox did not match its status")
                        return boot_id, session, sandbox
                sandbox_directories = list(session.glob("sandbox-*"))
                if len(sandbox_directories) != 1:
                    raise RuntimeError(f"{self.name} did not have exactly one sandbox directory")
                return boot_id, session, sandbox_directories[0]
            time.sleep(0.1)
        raise TimeoutError(f"{self.name} did not report readiness in {READY_TIMEOUT} seconds")

    def finish(self):
        if self.process.wait(timeout=READY_TIMEOUT) != 0:
            raise RuntimeError(f"{self.name} exited {self.process.returncode}")
        if "DONE" not in self.stdout.read_text(errors="replace").splitlines():
            raise RuntimeError(f"{self.name} did not finish its guest command")


def session_directories(sessions: Path):
    return set(sessions.glob("session-*"))


def process_identities(pids: set):
    if not pids:
        return set()
    result = subprocess.run(
        ["/bin/ps", "-p", ",".join(map(str, sorted(pids))), "-o", "pid=", "-o", "lstart=", "-o", "comm="],
        capture_output=True,
        text=True,
        timeout=3,
    )
    return {line.strip() for line in result.stdout.splitlines() if line.strip()}


def providers(sandbox: Path):
    disks = [str(path) for path in sandbox.glob("*.ext4")]
    if not disks:
        return set()
    try:
        result = subprocess.run(
            ["/usr/sbin/lsof", "-t", *disks], capture_output=True, text=True, timeout=4
        )
        pids = {int(line) for line in result.stdout.splitlines() if line.isdecimal()}
        return {
            line for line in process_identities(pids)
            if line.endswith(("/com.apple.Virtualization.VirtualMachine", " com.apple.Virtualization.VirtualMachine"))
        }
    except (OSError, subprocess.TimeoutExpired):
        return set()


def verify(binary: Path, assets: Path, work_parent: Path):
    with tempfile.TemporaryDirectory(prefix="nautilus-owner-exit-", dir=work_parent) as directory:
        work = Path(directory)
        sessions = work / ".nautilus-sessions"
        children = []

        def launch(name: str, program: str):
            stdout = work / f"{name}.stdout"
            stderr = work / f"{name}.stderr"
            with stdout.open("wb") as output, stderr.open("wb") as errors:
                process = subprocess.Popen(
                    [str(binary), "run", str(assets), str(work), BUSYBOX, "--", "/bin/sh", "-ec", program],
                    stdin=subprocess.DEVNULL,
                    stdout=output,
                    stderr=errors,
                )
            child = Workload(name, process, stdout, stderr)
            children.append(child)
            return child

        try:
            ready = """printf 'READY %s\\n' "$(cat /proc/sys/kernel/random/boot_id)" """
            first = launch("first", ready + "; sleep 600; echo DONE")
            first_boot, first_session, first_sandbox = first.start_result(sessions, set())
            second = launch("second", ready + "; sleep 25; echo DONE")
            second_boot, second_session, second_sandbox = second.start_result(sessions, {first_session})
            if first_boot == second_boot:
                raise RuntimeError("The two workloads reported the same guest boot ID")

            exclusive_providers = providers(first_sandbox) - providers(second_sandbox)
            if first.process.poll() is not None:
                raise RuntimeError("The first host process exited before SIGKILL")
            first.process.kill()
            if first.process.wait(timeout=10) != -signal.SIGKILL:
                raise RuntimeError("The first host process did not report SIGKILL")
            if not first_session.is_dir():
                raise RuntimeError("The first host did not leave a directory for stale recovery")

            third = launch("reaper", "echo DONE")
            deadline = time.monotonic() + 12
            while first_session.exists() and third.process.poll() is None and time.monotonic() < deadline:
                time.sleep(0.1)
            if first_session.exists():
                raise RuntimeError("The next invocation did not reclaim the killed workload's session")
            if not second_session.is_dir() or second.process.poll() is not None:
                raise RuntimeError("Stale-session recovery affected the second, live workload")

            if exclusive_providers:
                pids = {int(identity.split(maxsplit=1)[0]) for identity in exclusive_providers}
                deadline = time.monotonic() + 8
                while process_identities(pids) & exclusive_providers:
                    if time.monotonic() >= deadline:
                        raise RuntimeError(f"VZ provider process still running after owner exit: {sorted(pids)}")
                    time.sleep(0.15)
                print(f"VZ provider PIDs released after owner exit: {sorted(pids)}", flush=True)
            else:
                print("No dedicated Apple VZ provider PID was identifiable from open guest disks.", flush=True)

            third.finish()
            second.finish()
            remaining = session_directories(sessions)
            if remaining:
                raise RuntimeError(f"Session directories remain after normal exit: {sorted(remaining)}")
            print("PASS: SIGKILL owner, reap stale session, preserve live peer, and clean up normal exits")
        except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired):
            for child in children:
                for path in (child.stdout, child.stderr):
                    if path.exists():
                        print(f"{path.name}:\n{path.read_text(errors='replace')[-2000:]}", file=sys.stderr)
            raise
        finally:
            for child in reversed(children):
                if child.process.poll() is not None:
                    continue
                child.process.terminate()
                try:
                    child.process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.process.kill()
                    child.process.wait(timeout=10)


if __name__ == "__main__":
    if len(sys.argv) != 4:
        sys.exit("Usage: verify-owner-exit.py BINARY ASSET_DIR WORK_PARENT")
    try:
        verify(*(Path(argument).resolve(strict=True) for argument in sys.argv[1:]))
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as error:
        sys.exit(f"Owner-exit verification failed: {error}")
