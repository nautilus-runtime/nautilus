# OCI lifecycle performance

This benchmark measures how long a CRI client waits for an OCI workload to run on Nautilus and containerd. Run both on the same Linux host. The serial case measures individual request latency. The parallel case shows how that latency changes when several pods start together.

## Measurement boundaries

The driver uses a persistent CRI v1 gRPC channel to each Unix socket and a monotonic clock on the client. Each sample creates a new pod and container. The command writes a marker to a unique bind-mounted directory, then runs `sleep`. A sample succeeds only when the client sees the marker, CRI reports the container running, and stop and removal succeed.

| CSV column | Start | End |
| --- | --- | --- |
| `pod_ms` | Submit RunPodSandbox | Sandbox ID returned |
| `create_ms` | Sandbox ID returned | CreateContainer returns the container ID |
| `start_ms` | Container ID returned | StartContainer returns successfully |
| `create_start_ms` | Sandbox ID returned | StartContainer returns successfully |
| `ready_ms` | Submit RunPodSandbox | Client observes the workload marker after StartContainer returns |
| `wave_ms` | Dispatch a batch to one runtime | All its workloads pass checks and complete cleanup |

Use `create_start_ms` to compare startup within an existing sandbox. Compute it for each sample before taking percentiles. `ready_ms` includes sandbox creation and is an upper bound on first user code. The client polls at 1 ms after Start returns. Image pulls, client connection setup, kubelet scheduling and network configuration are outside these timing boundaries. Batch throughput includes teardown and can be computed as total measured workloads divided by the sum of distinct **per-wave** durations. Each row in a wave repeats the same duration.

## Test design

- **Workload and image identity.** Use a single-platform BusyBox manifest digest, the same command, namespace options and bind mount. Preload the image on both endpoints and containerd's sandbox image before warm-up. The driver checks that both CRI image services return the same config digest.
- **Host and isolation.** Use release binaries, two dedicated sockets and independent state, image and runtime directories. Use private network namespaces for the daemons. Configure systemd cgroups on both. The pod uses the daemon's network and IPC namespaces, a container PID namespace, no host ports and no CNI attachment. Keep the system services running. Record kernel, architecture, storage, CPU count, available memory and CPU pressure.
- **Execution.** Finish builds and image downloads before sampling. Run two adjacent waves per pair and alternate which runtime goes first. Exercise one runtime at a time. Use 50 measured pairs at concurrency 1 and 12 measured pairs at concurrency 4, after three warm-up pairs at each level. Keep warm-up rows in the CSV and exclude them from summaries.
- **Analysis.** Report completed/attempted counts, nearest-rank P50 and P95 in milliseconds, and the workload and concurrency beside each result. For nearest rank, sort the samples and take item `ceil(p × n)` (one-based). Compare the individual stages alongside end-to-end readiness because the runtimes can perform setup in different RPCs. Retain outliers. A failed RPC, missing marker, unexpected exit or cleanup failure invalidates the batch; investigate and record it before repeating.
- **Repeatability.** Repeat in separate time windows before setting a release threshold. Record image digest, runtime revisions and configuration changes; compare like-for-like runs. The first baseline covers one small image and a warm host filesystem cache.

## Running the comparison

Use two dedicated local CRI sockets, Python 3.10 or later and a shared host directory visible to the driver and both runtimes. The BusyBox example below uses a Linux ARM64 manifest. Select an appropriate platform digest on other architectures. From the repository root, set `NAUTILUS_SOCK` and `CONTAINERD_SOCK` to the absolute test socket paths.

```sh
bench=$(mktemp -d)
python3 -m venv "$bench/venv"
"$bench/venv/bin/pip" install grpcio==1.85.3 grpcio-tools==1.85.3
mkdir "$bench/proto" "$bench/probes"
"$bench/venv/bin/python" -m grpc_tools.protoc -I proto/cri-api/runtime/v1 \
  --python_out="$bench/proto" --grpc_python_out="$bench/proto" proto/cri-api/runtime/v1/api.proto

image=docker.io/library/busybox@sha256:bd44eb136a95dcc8dc58995e43abc40a413f2e8e3d4a2aae6bccbe94686acb05
for sock in "$NAUTILUS_SOCK" "$CONTAINERD_SOCK"; do
  sudo crictl --runtime-endpoint "unix://$sock" pull "$image"
done
for parallel in 1 4; do
  pairs=50
  if [ "$parallel" = 4 ]; then pairs=12; fi
  sudo env PYTHONPATH="$bench/proto" "$bench/venv/bin/python" scripts/benchmark_cri.py \
    --nautilus "$NAUTILUS_SOCK" --containerd "$CONTAINERD_SOCK" \
    --image "$image" --workdir "$bench/probes" --output "$bench/run-$parallel.csv" \
    --pairs "$pairs" --parallel "$parallel" --warmup 3
done
```

Preserve the CSVs before removing the temporary directory. The driver removes its labelled pods and containers. The operator owns the test daemons, their preloaded images and temporary storage.

## Additional validation dimensions

| Dimension | Follow-up experiment | Evidence to retain |
| --- | --- | --- |
| Cold image path | Reset only disposable runtime stores. Measure PullImage separately from first create/start. Use the same registry and platform digest. | Pull time, bytes, first readiness, subsequent warm runs |
| Image size and unpacking | Compare small and large images, then images with many layers. Use the same readiness command where possible. | Compressed size, layer count, storage driver, create and readiness P95 |
| Pod networking | Repeat with the same CNI configuration in isolated networks and verify pod connectivity. | Sandbox P95, DNS/connectivity result, IP lease cleanup |
| Concurrent capacity | Ramp batch size on a dedicated host. Record errors, resource saturation and recovery at each level. | Throughput, latency, CPU pressure, peak memory, I/O, remaining objects |
| Kubernetes path | Run the same Pod on equivalent kubelet nodes, one runtime per node. | Pod scheduled to Ready, image-pull events, container start and readiness timestamps |
