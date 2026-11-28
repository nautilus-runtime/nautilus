<h1 align="center"><a href="docs/assets/README.md"><img src="docs/assets/nautilus-logo-horizontal.png" width="318" alt="Nautilus"></a></h1>

<p align="center"><a href="https://nautilus-runtime.github.io/nautilus/">Project website</a></p>

Nautilus is a Linux container runtime written in Rust. It implements the Kubernetes Container Runtime Interface (CRI v1) and runs OCI containers, WebAssembly commands, and OCI containers inside a Pod-level VM. Kubelet and the bundled CLI use the same Unix socket, so you can run a workload locally before deploying it through Kubernetes.

## What you can run

| Workload | Runtime handler | Input |
| --- | --- | --- |
| Linux container | `runc` (default) | An OCI image from a registry. Nautilus pulls and caches the layers and runs the container with libcontainer. |
| WebAssembly command | `wasi` or `wasm` | A WASI Preview 1 module exporting `_start`, supplied as a local file or an OCI image. Wasmtime runs it inside the daemon. |
| Pod VM | `vm` (experimental, opt-in) | OCI images run inside one Linux guest per Pod. Requires KVM, Cloud Hypervisor and virtiofsd. |

OCI containers support CNI pod networking, host port mappings, resource limits, logs and exec. You can also load a Wasm policy to allow or deny pod creation, container creation or container start. The [example policy](examples/policy-deny-label) rejects workloads carrying the `nautilus.policy/deny` label.

## Why Nautilus

- **One runtime from local development to Kubernetes.** `nautilusd` implements CRI v1 directly. The bundled CLI and kubelet use the same socket, so you can try an OCI image locally with the same runtime handler you plan to use in Kubernetes.
- **VM isolation for existing OCI workloads.** Run an agent tool workspace in a dedicated Linux guest per Pod. Containers within a Pod share the guest kernel and Pod network. Repeated tool calls can use the same workspace for the life of the container.
- **Reuse image data across launches.** On supported hosts, Pod VM containers share a read-only OCI image snapshot and keep their own writable layers. Concurrent launches can reuse the unpacked image while keeping writes separate.

## Pod VM support

The experimental `vm` handler is disabled by default. To enable it, prepare a Linux KVM node with systemd, cgroup v2, Cloud Hypervisor, virtiofsd, and a guest kernel with a Nautilus workload initramfs. Follow the [node setup and RuntimeClass guide](docs/VM_RUNTIMECLASS.md) to select `runtimeClassName: nautilus-vm` in Kubernetes, or [run an OCI image locally with `--runtime vm`](#run-a-container-in-a-pod-vm).

On a real Kubernetes node, Pod VMs were tested with `kubectl exec`, `attach`, `logs`, and `port-forward`. The tests also checked that running guests retained their boot IDs and Pod IPs across a `nautilusd` restart. [Kubelet acceptance results](docs/CRITEST_RESULTS.md#2026-11-24-真实-kubelet-和-runtimeclass) · [VM lifecycle design](docs/VM_RUNTIME.md)

In a single-node benchmark with a pre-pulled Node.js image and 16 concurrent Pods, OCI snapshot reuse reduced the P95 time from starting a new Pod sandbox to the first successful exec from **9.90 s to 5.62 s**. [VM benchmarks and methodology](docs/VM_AGENT_LOOP.md#oci-rootfs-快照复用-ab)

## Quick start

### Install from GitHub Releases

On a Linux x86-64 or ARM64 machine with systemd and glibc 2.35 or newer, run:

```sh
curl -fsSL https://raw.githubusercontent.com/nautilus-runtime/nautilus/main/scripts/install.sh | bash
```

The installer downloads the matching archive from [GitHub Releases](https://github.com/nautilus-runtime/nautilus/releases), checks its SHA-256 checksum, installs both binaries and starts the systemd service on a fresh install. It installs missing networking tools and CNI plugins on Debian and Ubuntu through apt. On other distributions, install `ip`, `iptables` and the `bridge`, `loopback` and `host-local` CNI plugins before running it. The Pod VM handler has [separate node requirements](docs/VM_RUNTIMECLASS.md).

To install a specific release, add the tag after `bash -s --`. For example:

```sh
curl -fsSL https://raw.githubusercontent.com/nautilus-runtime/nautilus/v0.1.0/scripts/install.sh | bash -s -- v0.1.0
```

Rerun the installer to update the binaries. It preserves existing configuration and systemd unit files. After draining workloads, run `sudo systemctl restart nautilusd` to load the updated daemon. Check the installed service with `sudo nautilus version`.

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

The November 26, 2026 unfiltered `critest` v1.37.0 run completed with **107 passed, 0 failed and 35 skipped**. Kubernetes v1.37.2 has also exercised the VM RuntimeClass and native OCI CPU and memory resizing on a live node. On a KVM test node, new Pod VM exec and attach sessions succeeded on the first attempt after the guest connection recovered, and six consecutive 16-way agent-loop waves completed with no Pods or containers left. See the [conformance results](docs/CRITEST_RESULTS.md), [architecture guide](docs/ARCHITECTURE.md) and [roadmap](docs/DEVELOPMENT_ROADMAP.md) for coverage and remaining work.

The [containerd comparison](docs/PERFORMANCE.md) measures OCI pod sandbox creation, container creation, startup and workload readiness. It includes the test procedure and per-workload timings.

Contributions are welcome. To report a bug, include the output of `nautilus version`, the runtime handler and a minimal command that reproduces the behavior. [Open an issue](https://github.com/nautilus-runtime/nautilus/issues).

## License

Nautilus is licensed under the [Apache License 2.0](LICENSE).
