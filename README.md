<h1 align="center"><a href="docs/assets/README.md"><img src="docs/assets/nautilus-logo-horizontal.png" width="318" alt="Nautilus"></a></h1>

<p align="center"><a href="https://nautilus-runtime.github.io/nautilus/">Project website</a></p>

Nautilus is a Linux container runtime written in Rust. It implements the Kubernetes Container Runtime Interface (CRI v1) and runs OCI containers, WebAssembly commands, and OCI containers inside a Pod-level VM. Kubelet and the bundled CLI use the same Unix socket, so you can run a workload locally before deploying it through Kubernetes.

## What you can run

| Workload | Runtime handler | Input |
| --- | --- | --- |
| Linux container | `runc` (default) | An OCI image from a registry. Nautilus pulls and caches the layers and runs the container with libcontainer. |
| WebAssembly command | `wasi` or `wasm` | A WASI Preview 1 module exporting `_start`, supplied as a local file or an OCI image. Wasmtime runs it inside the daemon. |
| Pod VM | `vm` (opt-in) | OCI images run inside one Linux guest per Pod. Requires KVM, Cloud Hypervisor and virtiofsd. |

OCI containers support CNI pod networking, host port mappings, resource limits, logs and exec. You can also load a Wasm policy to allow or deny pod creation, container creation or container start. The [example policy](examples/policy-deny-label) rejects workloads carrying the `nautilus.policy/deny` label.

To enable the VM handler on a Kubernetes node and run a Pod, follow the [RuntimeClass guide](docs/VM_RUNTIMECLASS.md). The [VM design](docs/VM_RUNTIME.md) also covers the standalone boot probe and recovery tests. See the [CPU agent benchmark](docs/VM_AGENT_LOOP.md) for tool latency, memory per Pod and tested concurrency.

## Quick start

### Install from source

Use a Linux machine with systemd and a current stable Rust toolchain with Cargo. On Debian or Ubuntu, install the build and runtime dependencies and check that your Protocol Buffers compiler can read the vendored CRI schema.

```sh
sudo apt-get update
sudo apt-get install -y git curl ca-certificates python3 build-essential pkg-config protobuf-compiler libelf-dev zlib1g-dev libseccomp-dev libsystemd-dev runc iproute2 iptables containernetworking-plugins

git clone https://github.com/nautilus-runtime/nautilus.git
cd nautilus
make check-protoc
```

The schema uses the Protobuf `debug_redact` field option. Debian 13 ships `protoc` 3.21.12, which fails this check. If the compiler is missing or incompatible, install the pinned upstream 29.3 compiler for your user and select it for the build. The helper supports x86_64 and arm64 Linux.

```sh
bash scripts/setup_remote_env.sh --protoc-only
export PROTOC="$HOME/.local/share/nautilus/protoc-29.3/bin/protoc"
make check-protoc
```

Build and install the daemon, CLI and systemd service.

```sh
make install-service
```

This installs `nautilus`, the CLI, and `nautilusd`, the daemon. It also installs the systemd unit and a [default configuration](nautilus.toml) at `/etc/nautilus/config.toml`. Reinstalling preserves your existing configuration.

Before starting the service, check the paths in that file. Set `runtime.runc.path` to the output of `command -v runc`. Set `network.cni_bin_dir` to the directory containing the `bridge`, `loopback` and `host-local` plugins. The default is `/opt/cni/bin`. Debian packages commonly install them under `/usr/lib/cni`; you can locate them with `dpkg -L containernetworking-plugins`.

```sh
sudo systemctl start nautilusd
sudo nautilus version
```

### Run your first container

```sh
sudo nautilus run --rm busybox:1.36 echo 'Hello from Nautilus'
```

Nautilus pulls the image on first use and follows the command output. With `--rm`, the CLI waits for the command to finish and removes both the container and the sandbox it created. With `-d --rm`, the CLI prints the container ID and the daemon removes the workload and sandbox after exit. The image stays cached for the next run.

The CLI follows workload output and returns the workload exit code by default. Pass `-d` to start in the background and print the container ID.

Foreground OCI and VM runs can use `-i` to forward stdin, `-t` to allocate a
pseudo-terminal, or `-it` for an interactive shell. OCI attaches through the
default libcontainer driver. VM runs use the guest agent's init streams.
The first Attach can replay up to 16 KiB per output stream produced before
the connection opens. For VM startup output, use an updated guest agent.

```bash
printf 'hello\n' | sudo nautilus run -i --rm busybox:1.36 cat
sudo nautilus run -it --rm --runtime vm busybox:1.36 sh
```

```sh
sudo nautilus run -d --name demo busybox:1.36 sh -c 'while true; do echo tick; sleep 2; done'
sudo nautilus ps
sudo nautilus logs --tail 5 demo
sudo nautilus exec demo uname -m
sudo nautilus stop demo
sudo nautilus rm demo
```

### Run a WebAssembly command

If you have a WASI Preview 1 command compiled as `hello.wasm`, run it from the directory containing the module.

```sh
sudo nautilus run --rm --runtime wasi "file://$PWD/hello.wasm"
```

The daemon reads local modules from its own filesystem, and the CLI displays their stdout and stderr. OCI-packaged WASI commands write CRI log records. Use `-i` to pipe standard input to a foreground Preview 1 command. Use `-d` for background execution and add `--rm` to reclaim the workload when it exits.

For Kubernetes, package the module in an OCI image and use its absolute path as the entrypoint. Build for the Linux node platform. In this example, set `IMAGE` to a registry tag that your node can pull.

```sh
docker buildx build --platform linux/amd64 --push -t "$IMAGE" examples/wasi-command
kubectl apply -f examples/wasi-command/runtimeclass.yaml
kubectl run wasi-hello --image="$IMAGE" --restart=Never \
  --overrides='{"spec":{"runtimeClassName":"nautilus-wasi","automountServiceAccountToken":false}}'
kubectl wait --for=jsonpath='{.status.phase}'=Succeeded pod/wasi-hello --timeout=90s
kubectl logs wasi-hello
```

### Run a container in a Pod VM

On a node with the VM handler enabled, run a Linux image inside a new VM.

```sh
sudo nautilus run --rm --runtime vm --memory 512m --cpus 1 busybox:1.36 uname -r
```

Kubernetes workloads select the same handler using [RuntimeClass](docs/VM_RUNTIMECLASS.md).

## Use CRI tools

The default CRI endpoint is `unix:///run/nautilus/nautilus.sock`. With `crictl` installed, you can list the pod sandboxes on the same daemon.

```sh
sudo crictl --runtime-endpoint unix:///run/nautilus/nautilus.sock pods
```

For the Nautilus CLI, use the global `--socket` option or the `NAUTILUS_SOCKET` environment variable to select a different socket.

## Development

Run the Rust test suite on a Linux test machine with permission to create namespaces and network access to pull test images.

```sh
cargo test --locked --target "$(uname -m)-unknown-linux-gnu"
```

The November 25, 2026 unfiltered `critest` v1.37.0 run completed with **107 passed, 0 failed and 35 skipped**. Kubernetes v1.37.2 has also exercised the VM RuntimeClass and native OCI CPU and memory resizing on a live node. See the [conformance results](docs/CRITEST_RESULTS.md), [architecture guide](docs/ARCHITECTURE.md) and [roadmap](docs/DEVELOPMENT_ROADMAP.md) for coverage and remaining work.

The [containerd comparison](docs/PERFORMANCE.md) measures OCI pod sandbox creation, container creation, startup and workload readiness. It includes the test procedure and per-workload timings.

Contributions are welcome. To report a bug, include the output of `nautilus version`, the runtime handler and a minimal command that reproduces the behavior. [Open an issue](https://github.com/nautilus-runtime/nautilus/issues).

## License

Nautilus is licensed under the [Apache License 2.0](LICENSE).
