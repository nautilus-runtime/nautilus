#!/usr/bin/env python3
"""Verify that kubelet Attach connects to the original native OCI init."""

import argparse
import fcntl
import json
import os
import pty
import select
import signal
import struct
import subprocess
import termios
import time
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--node', required=True)
parser.add_argument('--socket', required=True, help='CRI endpoint used by kubelet')
parser.add_argument('--image', default='registry.k8s.io/e2e-test-images/busybox:1.29-2')
parser.add_argument('--kubectl', default='kubectl', help='uses KUBECONFIG')
parser.add_argument('--crictl', default='crictl')
args = parser.parse_args()
ns = 'init-attach-' + uuid.uuid4().hex[:8]
base = [args.kubectl]
k = [*base, '-n', ns]
crictl = [args.crictl, '--runtime-endpoint', args.socket, '--image-endpoint', args.socket]

def call(argv, **kwargs):
    return subprocess.run(argv, capture_output=True, check=True, timeout=100, **kwargs).stdout

def create(name, code, tty, once):
    spec = {'apiVersion': 'v1', 'kind': 'Pod', 'metadata': {'name': name},
            'spec': {'restartPolicy': 'Never', 'nodeName': args.node,
                     'containers': [{'name': 'init', 'image': args.image, 'imagePullPolicy': 'IfNotPresent',
                                     'command': ['/bin/sh', '-c', code], 'stdin': True,
                                     'stdinOnce': once, 'tty': tty}]}}
    call([*k, 'create', '-f', '-'], input=json.dumps(spec).encode())
    call([*k, 'wait', '--for=condition=Ready', '--timeout=85s', 'pod/' + name])

class TerminalAttach:
    def __enter__(self):
        self.master, slave = pty.openpty()
        self.resize(24, 80)
        self.client = subprocess.Popen([*k, 'attach', '-it', 'term'], stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        self.out = b''
        return self

    def resize(self, rows, cols):
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
        if hasattr(self, 'client'):
            self.client.send_signal(signal.SIGWINCH)

    def send(self, data):
        os.write(self.master, data)

    def expect(self, marker, repeat=None):
        end = time.monotonic() + 12
        while marker not in self.out:
            assert time.monotonic() < end, self.out
            if repeat:
                self.send(repeat)
            if select.select([self.master], [], [], 0.4)[0]:
                try:
                    self.out += os.read(self.master, 4096)
                except OSError as err:
                    raise RuntimeError(self.out) from err
        print(marker.decode(), flush=True)

    def __exit__(self, *exc):
        if self.client.poll() is None:
            self.client.terminate()
        self.client.wait(timeout=10)
        os.close(self.master)

call([*base, 'create', 'namespace', ns])
try:
    create('pipes', r'''printf 'pipe-ready\n'; printf 'pipe-error\n' >&2
IFS= read -r line
printf 'init=%s input=%s\n' "${$}" "$line"
IFS= read -r ignored || printf 'observed-eof\n'
exit 37''', False, True)
    result = subprocess.run([*k, 'attach', '-i', 'pipes'], input=b'pipe-token\n',
                            capture_output=True, timeout=45)
    assert result.returncode == 37, (result.returncode, result.stdout, result.stderr)
    assert b'pipe-ready' in result.stdout and b'init=1 input=pipe-token' in result.stdout, result.stdout
    assert b'observed-eof' in result.stdout, result.stdout
    assert b'pipe-error' in result.stderr, result.stderr
    logs = call([*k, 'logs', 'pipes'])
    assert all(item in logs for item in [b'pipe-ready', b'pipe-error', b'pipe-token', b'observed-eof']), logs
    print('pipes: init PID 1, startup stdout and stderr, stdin EOF, logs and exit 37', flush=True)
    create('term', r'''trap 'printf "init-sigint\n"; exit 42' INT
printf 'term-ready\n'
while IFS= read -r line; do
    printf 'init=%s input=%s size=%s\n' "${$}" "$line" "$(stty size)"
done''', True, False)
    with TerminalAttach() as first:
        first.expect(b'term-ready')
        first.expect(b'init=1 input=first size=24 80', repeat=b'first\n')
        first.resize(33, 101)
        first.expect(b'init=1 input=resized size=33 101', repeat=b'resized\n')
    phase = call([*k, 'get', 'pod', 'term', '-o', 'jsonpath={.status.phase}'])
    assert phase == b'Running', phase
    time.sleep(0.5)
    with TerminalAttach() as second:
        second.expect(b'init=1 input=reattached size=24 80', repeat=b'reattached\n')
        second.send(b'\x03')
        second.expect(b'init-sigint')
        assert second.client.wait(timeout=8) == 42, second.out
    print('terminal: reconnect to original init, Ctrl-C trap and exit 42', flush=True)
finally:
    call([*base, 'delete', 'namespace', ns, '--timeout=90s'])
    for _ in range(90):
        pods = json.loads(call([*crictl, 'pods', '--namespace', ns, '-o', 'json'])).get('items') or []
        if not pods:
            break
        time.sleep(1)
    assert not pods, pods
    containers = call([*crictl, 'ps', '-aq', '--label', f'io.kubernetes.pod.namespace={ns}'])
    assert not containers, containers
    print('CRI cleanup: no test Pod sandboxes or containers', flush=True)
