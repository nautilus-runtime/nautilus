"""Run crictl/client-go SPDY against real Pod VM containers created by the Rust fixture."""

import contextlib
import fcntl
import http.client
import os
import pty
import re
import select
import signal
import socket
import struct
import subprocess
import sys
import termios
import time
from pathlib import Path
from urllib.parse import urlsplit

crictl, endpoint, pod, target, console, token = sys.argv[1:]
base = [crictl, "--runtime-endpoint", "unix://" + endpoint]
shell = ["/bin/busybox", "sh", "-c"]


@contextlib.contextmanager
def child(args, **kwargs):
    p = subprocess.Popen(base + args, **kwargs)
    try:
        yield p
    finally:
        if p.poll() is None:
            p.kill()
        p.wait(timeout=5)


def until(fd, pattern, p):
    deadline = time.monotonic() + 10
    data = bytearray()
    while True:
        if (
            time.monotonic() >= deadline
            or not select.select([fd], [], [], max(0, deadline - time.monotonic()))[0]
        ):
            raise AssertionError(
                (
                    pattern,
                    bytes(data),
                    p.poll(),
                    p.stderr.read() if p.poll() is not None and p.stderr else None,
                )
            )
        try:
            part = os.read(fd, 8192)
        except OSError as e:
            raise AssertionError(
                (
                    pattern,
                    bytes(data),
                    p.poll(),
                    p.stderr.read() if p.poll() is not None and p.stderr else None,
                )
            ) from e
        if not part:
            raise AssertionError(
                (
                    pattern,
                    bytes(data),
                    p.poll(),
                    p.stderr.read() if p.poll() is not None and p.stderr else None,
                )
            )
        data.extend(part)
        m = re.search(pattern, data)
        if m:
            return m


def run(args, **kw):
    return subprocess.run(
        base + args, check=False, capture_output=True, timeout=15, **kw
    )


# Establish the upgrade against exactly the URL returned by CRI Exec. The
# second HTTP request must not open another session with this same token.
url = urlsplit(token)


def upgrade():
    conn = http.client.HTTPConnection(url.hostname, url.port, timeout=5)
    try:
        conn.request(
            "POST",
            url.path,
            headers={
                "Connection": "Upgrade",
                "Upgrade": "SPDY/3.1",
                "X-Stream-Protocol-Version": "v4.channel.k8s.io",
            },
        )
        return conn.getresponse().status
    except http.client.RemoteDisconnected:
        return 0
    finally:
        conn.close()


assert upgrade() == 101 and upgrade() != 101, "streaming URL was not single-use"

# Detach the owning guest transport while the Pod and its CRI endpoint are alive.
transport = Path(endpoint).parent / "state" / "vms" / pod / "vsock.sock"
detached = transport.with_name("spdy-offline.sock")
transport.rename(detached)
try:
    result = run(["exec", target, *shell, "echo unexpected"])
finally:
    detached.rename(transport)
assert result.returncode != 0 and (
    b"guest" in result.stderr.lower() or b"vsock" in result.stderr.lower()
), result

# Wait for output before writing input, then check stream separation and status.
with child(
    ["exec", "-i", target, *shell, 'echo read-now; read v; echo "err-$v" >&2; exit 35'],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
) as p:
    until(p.stdout.fileno(), b"read-now", p)
    stdout, stderr = p.communicate(b"stream\n", timeout=10)
    assert (
        b"err-stream" in stderr and b"err-stream" not in stdout and p.returncode != 0
    ), (stdout, stderr, p.returncode)
    assert p.returncode == 35 or b"35" in stderr, (stderr, p.returncode)


# Binary stdio spans many SPDY and vsock frames in both directions.
payload = bytes(range(256)) * (12 * 1024)
echo = subprocess.run(
    base + ["exec", "-i", target, "/bin/busybox", "cat"],
    input=payload, capture_output=True, timeout=20, check=True,
)
assert echo.stdout == payload, (len(payload), len(echo.stdout))


def terminal(args, exchange):
    master, slave = pty.openpty()
    try:
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        with child(
            args, stdin=slave, stdout=slave, stderr=slave, start_new_session=True
        ) as p:
            exchange(master, p)
    finally:
        os.close(master)
        os.close(slave)


def resize(fd, p):
    until(fd, b"tty-ready", p)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 39, 121, 0, 0))
    p.send_signal(signal.SIGWINCH)
    os.write(fd, b"continue\n")
    until(fd, b"39 121", p)
    assert p.wait(timeout=10) == 0


terminal(
    [
        "exec",
        "-i",
        "-t",
        target,
        *shell,
        'test -t 0 && echo tty-ready; read v; for i in 1 2 3 4 5 6 7 8 9 10; do size=$(/bin/busybox stty size); test "$size" = "39 121" && break; /bin/busybox sleep .1; done; echo "$size"',
    ],
    resize,
)

for count in [1, 2]:

    def attach(fd, p, count=count):
        # stdin is live even when the client doesn't replay historical output.
        os.write(fd, b"attach\n")
        until(fd, f"original-1-{count}-attach".encode(), p)

    terminal(["attach", "-i", "-t", console], attach)

with child(
    [
        "exec",
        target,
        *shell,
        "echo started; /bin/busybox sleep 1; echo leaked > spdy-cancel",
    ],
    stdout=subprocess.PIPE,
) as p:
    until(p.stdout.fileno(), b"started", p)
time.sleep(1.2)
assert run(["exec", "--sync", target, *shell, "test ! -e spdy-cancel"]).returncode == 0

with child(
    ["port-forward", pod, ":18080", ":18081"],
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
) as p:
    # Each remote port is bound at a dynamically allocated TCP port in this isolated host netns.
    listeners = until(
        p.stdout.fileno(),
        rb"(?s)(?=.*?Forwarding from 127\.0\.0\.1:(\d+) -> 18080)(?=.*?Forwarding from 127\.0\.0\.1:(\d+) -> 18081)",
        p,
    )
    ports = {18080: int(listeners[1]), 18081: int(listeners[2])}
    for _ in range(2):
        with socket.create_connection(("127.0.0.1", ports[18080]), timeout=5) as s:
            s.sendall(b"GET / HTTP/1.0\r\n\r\n")
            data = bytearray()
            while chunk := s.recv(8192):
                data.extend(chunk)
            assert b"guest-spdy-only" in data, data
    with socket.create_connection(("127.0.0.1", ports[18081]), timeout=5) as s:
        try:
            s.sendall(b"refused")
            assert not s.recv(100)
        except ConnectionResetError:
            pass
    until(p.stdout.fileno(), rb"(refused|connect to guest port 18081)", p)
print(
    "CRI SPDY crictl exec, resize, original init attach, cancellation and guest port forwarding passed",
    flush=True,
)
