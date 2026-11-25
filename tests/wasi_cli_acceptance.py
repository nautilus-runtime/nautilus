import argparse
import os
from pathlib import Path
import selectors
import signal
import subprocess
import uuid

parser = argparse.ArgumentParser(description="Exercise WASI pipe Attach and --rm through Nautilus CLI")
parser.add_argument("--client", required=True)
parser.add_argument("--socket", required=True, help="Unix filesystem path")
parser.add_argument("--module", required=True, type=Path, help="pipe_stdio.wat on the daemon host")
args = parser.parse_args()
cli = [args.client, "--socket", args.socket]
suffix = uuid.uuid4().hex[:8]
module = args.module.resolve().as_uri()

def call(*argv, **kwargs):
    return subprocess.run([*cli, *argv], capture_output=True, timeout=20, **kwargs)

def assert_removed(name):
    result = call("ps", "-a", check=True)
    assert name.encode() not in result.stdout, result.stdout

for i, payload in enumerate([b"", b"x", bytes(range(251)) * 120] * 3):
    name = f"wasi-pipe-{suffix}-{i}"
    handler = "wasi" if i % 2 == 0 else "wasm"
    result = call("run", "--runtime", handler, "-i", "--rm", "--name", name, module, input=payload)
    assert result.returncode == 37, (result.returncode, result.stderr)
    assert result.stdout == payload and b"ready\n" in result.stderr, (len(result.stdout), result.stderr)
    assert_removed(name)
print("9 WASI runs preserved binary stdout, separate stderr, exit 37, and --rm", flush=True)

name = f"wasi-cancel-{suffix}"
with subprocess.Popen([*cli, "run", "--runtime", "wasi", "-i", "--rm", "--name", name, module],
                      stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
    selector = selectors.DefaultSelector()
    try:
        selector.register(process.stderr, selectors.EVENT_READ)
        assert selector.select(12), "WASI process did not reach its stdin wait"
        stderr = os.read(process.stderr.fileno(), 4096)
        assert b"ready\n" in stderr, stderr
        process.send_signal(signal.SIGINT)
        # Keep stdin open until the signal path has stopped the module.
        assert process.wait(timeout=12) == 130, process.stderr.read()
    finally:
        selector.close()
        if process.poll() is None:
            process.kill()
        process.wait(timeout=5)
assert_removed(name)
print("SIGINT stopped the blocked WASI run, returned 130, and removed the container", flush=True)

name = f"wasi-no-tty-{suffix}"
result = call("run", "--runtime", "wasi", "-it", "--rm", "--name", name, module, input=b"")
assert result.returncode != 0 and b"WASI supports piped stdin" in result.stderr, result.stderr
assert_removed(name)
print("WASI TTY rejected before creating a workload", flush=True)
