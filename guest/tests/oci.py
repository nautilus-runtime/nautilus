#!/usr/bin/env python3
"""Run against a live Cloud Hypervisor guest with a dedicated podfs directory."""

import argparse
import base64
import json
import shutil
import socket
import struct
import time
import uuid
from pathlib import Path


def uid():
    return str(uuid.uuid4())


def read_exact(sock, size):
    buf = bytearray()
    while len(buf) < size:
        chunk = sock.recv(size - len(buf))
        if not chunk:
            raise EOFError("guest closed the connection")
        buf.extend(chunk)
    return buf


def frame(sock):
    (size,) = struct.unpack(">I", read_exact(sock, 4))
    assert 0 < size <= 1048576
    return json.loads(read_exact(sock, size))


def connect(port):
    sock = socket.socket(socket.AF_UNIX)
    sock.settimeout(10)
    sock.connect(args.socket)
    sock.sendall(f"CONNECT {port}\n".encode())
    ack = bytearray()
    while not ack.endswith(b"\n") and len(ack) < 64:
        ack.extend(read_exact(sock, 1))
    assert ack.startswith(b"OK "), ack
    return sock


def call(op, cid, **options):
    with connect(4083) as sock:
        hello = frame(sock)
        assert hello == identity
        req = dict(identity, requestId=uid(), containerId=cid, op=op)
        req.update(options)
        data = json.dumps(req).encode()
        sock.sendall(struct.pack(">I", len(data)) + data)
        response = frame(sock)
        assert response["requestId"] == req["requestId"], response
        return response


def ok(op, cid, **kw):
    res = call(op, cid, **kw)
    assert res["ok"], res
    return res


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("socket")
    p.add_argument("share", type=Path)
    p.add_argument("busybox", type=Path)
    args = p.parse_args()
    until = time.monotonic() + 25
    while True:
        try:
            with connect(4083) as sock:
                frame(sock)
            break
        except (OSError, EOFError):
            if time.monotonic() >= until:
                raise
            time.sleep(0.1)
    with connect(4082) as s:
        s.sendall(b"IDENTIFY 1\n")
        _, v, pod, nonce, boot = s.makefile("rb").readline().decode().split()
    identity = {"version": int(v), "sandboxId": pod, "nonce": nonce, "bootId": boot}
    assert call("status", uid())["code"] == "not_found"
    assert call("remove", uid(), nonce=uid())["code"] == "identity"
    assert call("remove", uid(), version=42)["code"] == "invalid"
    print("OCI v1 handshake, identity, and missing container passed", flush=True)

    first, second = uid(), uid()

    def bundle(cid, program):
        d = args.share / "containers" / cid
        root = d / "rootfs"
        for path in ["bin", "dev", "proc", "etc", "workspace"]:
            (root / path).mkdir(parents=True, exist_ok=True)
        shutil.copy2(args.busybox, root / "bin/busybox")
        for cmd in [
            "sh",
            "cat",
            "sleep",
            "wc",
            "ls",
            "readlink",
            "mknod",
            "setsid",
            "id",
            "tr",
            "head",
        ]:
            (root / "bin" / cmd).symlink_to("busybox")
        config = {
            "ociVersion": "1.1.0",
            "root": {"path": "rootfs"},
            "process": {
                "user": {"uid": 0, "gid": 0},
                "args": ["/bin/sh", "-c", program],
                "env": ["PATH=/bin", "TEST_ENV=fixture"],
                "cwd": "/workspace",
                "noNewPrivileges": True,
            },
            "linux": {"namespaces": [{"type": "mount"}, {"type": "pid"}]},
        }
        (d / "config.json").write_text(json.dumps(config))
        return d

    d = bundle(first, "echo firstout; echo firsterr >&2; exec sleep 120")
    invalid = uid()
    broken = bundle(invalid, "exit 0")
    specfile = broken / "config.json"
    original = json.loads(specfile.read_text())
    for bad in [
        {"root": {"path": "../"}},
        {"hooks": {}},
        {"linux": {"namespaces": [{"type": "mount"}]}},
        {
            "mounts": [
                {
                    "type": "bind",
                    "source": "/etc",
                    "destination": "/etc",
                    "options": ["bind", "rw"],
                }
            ]
        },
        {
            "process": dict(
                original["process"], capabilities={"effective": ["CAP_SYS_ADMIN"]}
            )
        },
    ]:
        specfile.write_text(json.dumps(dict(original, **bad)))
        assert not call("create", invalid)["ok"], bad
    specfile.unlink()
    specfile.symlink_to("../" + first + "/config.json")
    assert not call("create", invalid)["ok"]
    assert call("remove", "../" + first)["code"] == "invalid"
    bundle(second, "echo secondout; while test ! -e finish; do sleep .1; done; exit 37")
    # A read-only Pod volume is mounted inside the first workload.
    vol = uid()
    source = args.share / "volumes" / vol
    source.parent.mkdir()
    source.write_text("from pod volume")
    config = json.loads((d / "config.json").read_text())
    config["mounts"] = [
        {
            "type": "bind",
            "source": "volumes/" + vol,
            "destination": "/workspace/volume",
            "options": ["bind", "ro"],
        }
    ]
    config["linux"]["resources"] = {
        "cpu": {"quota": 50000, "period": 100000},
        "memory": {"limit": 64 << 20},
    }
    (d / "config.json").write_text(json.dumps(config))
    for cid in [first, second]:
        assert ok("create", cid)["state"] == "created"
        assert ok("create", cid)["state"] == "created"
        assert ok("start", cid)["startedAt"] > 0

    def execute(cid, program, timeout=5000):
        return ok("exec", cid, argv=["/bin/sh", "-c", program], timeoutMs=timeout)

    assert (
        execute(
            first, 'test "$(cat volume)" = "from pod volume" && ! (echo wrong > volume)'
        )["exitCode"]
        == 0
    )
    assert execute(second, "test ! -e volume")["exitCode"] == 0
    execute(first, "echo shared > /dev/shm/pod")
    assert execute(second, 'test "$(cat /dev/shm/pod)" = shared')["exitCode"] == 0
    for cid in [first, second]:
        assert (
            base64.b64decode(
                execute(cid, "cat /proc/sys/kernel/random/boot_id")["stdout"]
            )
            .decode()
            .strip()
            == boot
        )
    for i in range(10):
        res = execute(first, "echo x >> counter; wc -l < counter")
        assert res["exitCode"] == 0 and int(base64.b64decode(res["stdout"])) == i + 1, (
            res
        )
    res = execute(first, "echo $TEST_ENV; echo separate >&2; exit 41", 0)
    assert (
        base64.b64decode(res["stdout"]) == b"fixture\n"
        and base64.b64decode(res["stderr"]) == b"separate\n"
        and res["exitCode"] == 41
    ), res
    assert execute(second, "test ! -e counter")["exitCode"] == 0

    def ns(cid):
        return base64.b64decode(execute(cid, "readlink /proc/self/ns/pid")["stdout"])

    assert ns(first) != ns(second)
    assert (
        execute(
            first,
            "test ! -e /dev/kvm && test ! -e /dev/vsock && ! mknod /dev/custom c 1 3",
        )["exitCode"]
        == 0
    )
    assert (
        call(
            "exec",
            first,
            argv=[
                "/bin/sh",
                "-c",
                "setsid sh -c 'sleep .5; echo leak > orphan' & sleep 30",
            ],
            timeoutMs=75,
        )["code"]
        == "timeout"
    )
    assert (
        call(
            "exec",
            first,
            argv=["/bin/sh", "-c", "head -c 300000 /dev/zero"],
            timeoutMs=3000,
        )["code"]
        == "limit"
    )
    with connect(4083) as sock:
        assert frame(sock) == identity
        data = json.dumps(
            dict(
                identity,
                requestId=uid(),
                containerId=first,
                op="exec",
                argv=[
                    "/bin/sh",
                    "-c",
                    "echo started > disconnect; sleep 2; echo leak > disconnected",
                ],
                timeoutMs=0,
            )
        ).encode()
        sock.sendall(struct.pack(">I", len(data)) + data)
        until = time.monotonic() + 2
        while not (d / "rootfs/workspace/disconnect").exists():
            assert time.monotonic() < until
            time.sleep(0.025)
    time.sleep(2.1)
    assert execute(first, "test ! -e disconnected")["exitCode"] == 0
    # A successful foreground command may leave a child holding both pipes.
    # The exec result must still be delivered, and its cgroup must be cleaned.
    completed = execute(first, "(sleep 2; echo leak > escaped) & echo done")
    assert base64.b64decode(completed["stdout"]) == b"done\n"
    time.sleep(2.1)
    assert execute(first, "test ! -e escaped")["exitCode"] == 0
    assert execute(first, "test ! -e orphan")["exitCode"] == 0
    execute(second, "echo done > finish")
    until = time.monotonic() + 8
    while ok("status", second)["state"] != "exited":
        assert time.monotonic() < until
        time.sleep(0.05)
    status = ok("status", second)
    assert (
        status["exitCode"] == 37
        and 0 < status["createdAt"] <= status["startedAt"] <= status["finishedAt"]
    ), status
    offset = 0
    data = b""
    while True:
        chunk = ok("readLog", first, offset=offset, limit=13)
        piece = base64.b64decode(chunk.get("stdout", ""))
        assert chunk["nextOffset"] == offset + len(piece)
        if not piece:
            break
        data += piece
        offset = chunk["nextOffset"]
    assert (
        b" stdout F firstout\n" in data
        and b" stderr F firsterr\n" in data
        and b"secondout" not in data
    ), data
    for cid in [first, second]:
        assert ok("stop", cid, timeoutMs=0)["state"] == "exited"
        ok("stop", cid, timeoutMs=0)
        ok("remove", cid)
        ok("remove", cid)
    print(
        "Two bundles, persistent exec, isolated root/PID/devices, logs, exit, timeout and retry passed",
        flush=True,
    )
