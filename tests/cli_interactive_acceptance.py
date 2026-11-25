#!/usr/bin/env python3
"""Run the live foreground CLI against an isolated Nautilus daemon."""
import argparse
import fcntl
import os
import pty
import select
import signal
import subprocess
import struct
import tempfile
import termios
import time
import uuid

parser = argparse.ArgumentParser()
parser.add_argument("--client", required=True)
parser.add_argument("--socket", required=True)
parser.add_argument("--image", required=True)
parser.add_argument("--runtimes", nargs="+", choices=["runc", "vm"], default=["runc", "vm"])
args = parser.parse_args()
cli = [args.client, "--socket", args.socket.removeprefix("unix://")]
prefix = "interactive-" + uuid.uuid4().hex[:8]
names = []


def command(runtime, label, mode, script):
    name = f"{prefix}-{runtime}-{label}"
    names.append(name)
    return [
        *cli, "run", mode, "--rm", "--name", name, "--runtime", runtime,
        *(["--cpus", "1", "--memory", "512m"] if runtime == "vm" else []),
        "--entrypoint", "/bin/sh", args.image, "--", "-c", script,
    ]


def run(runtime, label, mode, script, data=b""):
    return subprocess.run(
        command(runtime, label, mode, script), input=data, capture_output=True, timeout=120,
    )


def check_cleanup(runtime):
    inventory = subprocess.check_output([*cli, "ps", "-a"], timeout=30)
    assert not any(name.encode() in inventory for name in names), inventory
    print(f"{runtime}: --rm left no test containers", flush=True)


def blocked_output_run():
    script = r"""trap 'exit 0' TERM
yes foreground-output | head -c 1048576
while :; do sleep 1; done
"""
    with subprocess.Popen(command("runc", "blocked-output", "-i", script),
                          stdin=subprocess.DEVNULL, stdout=subprocess.PIPE) as child:
        try:
            fd = child.stdout.fileno()
            capacity = fcntl.fcntl(fd, fcntl.F_GETPIPE_SZ)
            deadline = time.monotonic() + 15
            while struct.unpack("I", fcntl.ioctl(fd, termios.FIONREAD, bytes(4)))[0] < capacity:
                assert child.poll() is None and time.monotonic() < deadline
                time.sleep(.02)
            child.send_signal(signal.SIGINT)
            assert child.wait(timeout=20) == 130
        finally:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=5)
    print("runc: SIGINT stopped and removed the run with a full stdout pipe", flush=True)


def terminal_run(runtime):
    script = r'''
trap 'printf "remote-interrupt\n"; exit 42' INT
printf 'tty-ready\n'
while IFS= read -r line; do
    printf 'init=%s input=%s size=%s\n' "$$" "$line" "$(stty size)"
done
'''
    master, slave = pty.openpty()
    child = None
    observed = bytearray()
    initial_mode = termios.tcgetattr(slave)

    def expect(marker, repeat=None, timeout=30):
        deadline = time.monotonic() + timeout
        while marker not in observed:
            assert time.monotonic() < deadline, bytes(observed)
            if repeat is not None and child.poll() is None:
                os.write(master, repeat)
            if select.select([master], [], [], 0.25)[0]:
                observed.extend(os.read(master, 4096))
            elif child.poll() is not None:
                raise RuntimeError((child.returncode, bytes(observed)))

    try:
        termios.tcsetwinsize(slave, (24, 80))
        child = subprocess.Popen(
            command(runtime, "tty", "-it", script),
            stdin=slave, stdout=slave, stderr=slave, start_new_session=True,
        )
        expect(b"tty-ready", timeout=90)
        expect(b"init=1 input=before size=24 80", repeat=b"before\n")
        termios.tcsetwinsize(slave, (33, 117))
        child.send_signal(signal.SIGWINCH)
        expect(b"init=1 input=after size=33 117", repeat=b"after\n")
        os.write(master, b"\x03")
        expect(b"remote-interrupt")
        assert child.wait(20) == 42, bytes(observed)
        assert termios.tcgetattr(slave) == initial_mode, "client did not restore the local terminal"
        print(f"{runtime}: TTY resize, remote Ctrl-C and exit 42, terminal restored", flush=True)
    finally:
        if child is not None and child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
        os.close(slave)
        os.close(master)


try:
    for runtime in args.runtimes:
        pipe = run(runtime, "pipe", "-i", r'''
printf 'early-stdout\n'
printf 'early-stderr\n' >&2
IFS= read -r line
printf 'init=%s input=%s\n' "$$" "$line"
IFS= read -r rest || printf 'saw-eof\n'
exit 37
''', b"pipe-token\n")
        assert pipe.returncode == 37, (pipe.returncode, pipe.stdout, pipe.stderr)
        assert pipe.stdout == b"early-stdout\ninit=1 input=pipe-token\nsaw-eof\n", pipe.stdout
        assert pipe.stderr == b"early-stderr\n", pipe.stderr
        print(f"{runtime}: original init, stdout/stderr, stdin EOF and exit 37", flush=True)

        binary = bytes(range(256)) * 1024
        copied = run(runtime, "binary", "-i", "exec cat", binary)
        assert copied.returncode == 0 and copied.stdout == binary, (
            copied.returncode, len(copied.stdout), copied.stderr,
        )
        print(f"{runtime}: {len(binary)} binary bytes round-tripped", flush=True)

        if runtime == "runc":
            with tempfile.TemporaryFile() as source:
                source.write(b"consumed:remaining")
                source.seek(len(b"consumed:"))
                copied = subprocess.run(command(runtime, "file-offset", "-i", "exec cat"),
                                        stdin=source, capture_output=True, timeout=30)
                assert (copied.returncode, copied.stdout) == (0, b"remaining"), copied
            blocked_output_run()

        quick = run(runtime, "fast", "-i", "printf fast; printf error >&2; exit 31")
        assert (quick.returncode, quick.stdout, quick.stderr) == (31, b"fast", b"error"), (
            quick.returncode, quick.stdout, quick.stderr,
        )
        print(f"{runtime}: startup output and quick exit 31 preserved", flush=True)

        tty_output = run(runtime, "tty-out", "-t", "test -t 1 && printf terminal; exit 23")
        assert (tty_output.returncode, tty_output.stdout, tty_output.stderr) == (23, b"terminal", b""), (
            tty_output.returncode, tty_output.stdout, tty_output.stderr,
        )
        terminal_run(runtime)
        check_cleanup(runtime)
finally:
    inventory = subprocess.check_output([*cli, "ps", "-a"], timeout=30)
    for name in names:
        if name.encode() in inventory:
            subprocess.run([*cli, "rm", "-f", name], check=True, timeout=30)
