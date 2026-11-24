"""Exercise the authenticated streaming upgrade on a real guest vsock."""

import base64
import json
import struct
import time


def run(connect, frame, read_exact, identity, uid, bundle, ok, first):
    def open_stream(op, cid, options, argv=None, success=True):
        sock = connect(4083)
        assert frame(sock) == identity
        req = dict(identity, requestId=uid(), containerId=cid, op=op, stream=options)
        if argv is not None:
            req["argv"] = argv
        data = json.dumps(req).encode()
        sock.sendall(struct.pack(">I", len(data)) + data)
        reply = frame(sock)
        assert reply["requestId"] == req["requestId"] and reply["ok"] == success, reply
        return sock

    def send(sock, tag, data=b""):
        sock.sendall(struct.pack(">BI", tag, len(data)) + data)

    def recv(sock):
        tag, size = struct.unpack(">BI", read_exact(sock, 5))
        assert size <= 65536
        return tag, bytes(read_exact(sock, size))

    def until(sock, expected, channel=1):
        buf = bytearray()
        while expected not in buf:
            tag, data = recv(sock)
            assert tag == channel, (tag, data)
            buf.extend(data)
        return bytes(buf)

    with open_stream(
        "execStream",
        first,
        {"stdin": True, "stdout": True, "stderr": True},
        ["/bin/sh", "-c", 'echo before; read v; echo "$v" >&2; exit 35'],
    ) as sock:
        assert b"before" in until(sock, b"before")
        send(sock, 0, b"sent live\n")
        assert b"sent live" in until(sock, b"sent live", 2)
        assert recv(sock) == (6, struct.pack(">i", 35))

    with open_stream(
        "execStream",
        first,
        {"stdin": True, "stdout": True, "tty": True},
        [
            "/bin/sh",
            "-c",
            "test -t 0 && echo tty-ready; read v; /bin/busybox stty size",
        ],
    ) as sock:
        until(sock, b"tty-ready")
        send(sock, 4, struct.pack(">HH", 111, 37))
        send(sock, 0, b"go\n")
        until(sock, b"37 111")
        assert recv(sock) == (6, struct.pack(">i", 0))

    sock = open_stream(
        "execStream",
        first,
        {"stdout": True},
        ["/bin/sh", "-c", "echo started; sleep 1; echo leak > stream-leak"],
    )
    until(sock, b"started")
    sock.close()
    time.sleep(1.1)
    assert (
        ok(
            "exec",
            first,
            argv=["/bin/sh", "-c", "test ! -e stream-leak"],
            timeoutMs=3000,
        )["exitCode"]
        == 0
    )

    for tty in [False, True]:
        cid = uid()
        d = bundle(
            cid, 'n=0; while read word; do n=$((n+1)); echo "init-$$-$n-$word"; done'
        )
        config = json.loads((d / "config.json").read_text())
        config["annotations"] = {"io.nautilus.stdin": "true"}
        config["process"]["terminal"] = tty
        (d / "config.json").write_text(json.dumps(config))
        ok("create", cid)
        ok("start", cid)
        for n in (1, 2):
            with open_stream(
                "attach", cid, {"stdin": True, "stdout": True, "tty": tty}
            ) as sock:
                send(sock, 0, b"hello\n")
                until(sock, f"init-1-{n}-hello".encode())
        assert ok("status", cid)["state"] == "running"
        ok("remove", cid)

    cid = uid()
    bundle(
        cid,
        "echo guest-only > index.html; exec /bin/busybox httpd -f -p 127.0.0.1:18080 -h /workspace",
    )
    ok("create", cid)
    ok("start", cid)
    ready = ok(
        "exec",
        first,
        argv=[
            "/bin/sh",
            "-c",
            "for i in 1 2 3 4 5 6 7 8 9 10; do /bin/busybox wget -qO- http://127.0.0.1:18080/ && exit; sleep 0.1; done; /bin/busybox ip addr show lo; exit 1",
        ],
        timeoutMs=5000,
    )
    assert ready["exitCode"] == 0, (
        base64.b64decode(ready.get("stdout", "")),
        ok("status", cid),
        ok("readLog", cid, offset=0, limit=1024),
    )
    for _ in range(2):
        with open_stream("portForward", identity["sandboxId"], {"port": 18080}) as sock:
            send(sock, 0, b"GET / HTTP/1.0\r\n\r\n")
            send(sock, 5)
            until(sock, b"guest-only")
            assert recv(sock) == (6, struct.pack(">i", 0))
    open_stream(
        "portForward", identity["sandboxId"], {"port": 18081}, success=False
    ).close()
    ok("remove", cid)
    print(
        "Streaming exec, real PTY resize, init attach/reconnect, cancellation and guest port forward passed",
        flush=True,
    )
