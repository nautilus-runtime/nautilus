# OCI lifecycle performance

This benchmark measures how long a CRI client waits for an OCI workload to run on Nautilus and containerd. Run both on the same Linux host. The serial case measures individual request latency. The parallel case shows how that latency changes when several pods start together.

## Independent repeat · 2026-11-24

With the BusyBox image already unpacked, Nautilus completed CreateContainer + StartContainer in **45.7 ms at P95** serially and **80.4 ms** with four concurrent pods. These were 33% and 38% lower than containerd 2.2.2 in the same window. The previous optimized run measured 47.2 and 91.1 ms for Nautilus. The runtime source and locked dependencies were identical at the two tested revisions, `b801cc1` and `240e461`. The difference between windows gives an indication of run-to-run variation. [Raw samples from the repeat](benchmarks/cri-lifecycle-arm64-repeat.csv)

Each latency cell shows **P50 / P95 in milliseconds**. Each runtime completed 50 serial workloads and 48 workloads in 12 batches of four after three warm-up pairs at each concurrency. Throughput counts full lifecycles, including status checks and removal.

| Concurrent pods | Runtime | Pod sandbox | Create container | Start container | Create + start | Pod to ready | Workloads/s |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | Nautilus | 7.4 / 8.3 | 10.8 / 12.7 | 32.5 / 34.6 | 43.5 / 45.7 | 51.5 / 53.8 | 7.85 |
| 1 | containerd | 68.2 / 76.9 | 13.5 / 16.4 | 48.4 / 54.0 | 62.9 / 68.4 | 131.4 / 138.9 | 4.39 |
| 4 | Nautilus | 14.0 / 17.1 | 17.0 / 23.4 | 44.1 / 60.8 | 62.8 / 80.4 | 77.8 / 96.4 | 23.73 |
| 4 | containerd | 130.4 / 154.5 | 30.1 / 47.9 | 78.8 / 93.8 | 109.5 / 129.4 | 242.3 / 272.4 | 9.63 |

### First use with compressed layers cached

Nautilus built its shared image tree during the first CreateContainer. containerd unpacked during PullImage. For a caller that has already pulled the image, Nautilus took **136.3 ms** at P95 for the first create + start, versus **79.5 ms** for containerd. Counting both the cached pull and the time from pod submission to workload readiness, Nautilus took **151.2 ms** at P95 versus **219.1 ms** for containerd. The last column sums the two intervals within each sample before computing percentiles.

| Runtime | Cached pull | First create | First create + start | Pod to ready | Cached pull + pod to ready |
| --- | ---: | ---: | ---: | ---: | ---: |
| Nautilus | 1.2 / 1.3 | 98.2 / 101.0 | 132.6 / 136.3 | 146.2 / 149.9 | 147.5 / 151.2 |
| containerd | 69.7 / 74.1 | 13.1 / 16.0 | 66.4 / 79.5 | 138.6 / 146.3 | 208.9 / 219.1 |

The runtimes alternated order over 15 serial pairs. Before each sample the driver removed the BusyBox image record, checked that its unpacked tree was absent and verified the cached layer digest. On containerd it also pinned the compressed content against garbage collection and removed the BusyBox snapshot reference and snapshot. Nautilus read the cached manifest locally. containerd resolved the pinned manifest through a loopback registry and transferred no layer data. Snapshot checks confirmed when each runtime unpacked the image. With 15 samples, P95 is the largest observation. The result covers a warm filesystem cache and one 1.90 MB compressed layer.

Both dedicated daemons used the same four ARM64 CPUs, Linux 6.8, ext4 and an isolated systemd manager. Nautilus used the locked release build with rustc 1.98.0 and in-process youki 0.8.0. containerd used its runc v2 shim, runc 1.3.4 and OverlayFS. CPU pressure (`some avg10`) before the serial, four-way and first-use series was 0.00%, 2.65% and 15.34% respectively. All workloads passed readiness, running-state and cleanup checks. Both test endpoints were empty after the run.

## OCI snapshots and youki 0.8.0 · 2026-11-24

The optimized runtime completed serial warm-image CreateContainer + StartContainer in **47.2 ms at P95**, down from **101.3 ms**. Both runs used the same pinned BusyBox image. The previous runtime unpacked the image for every container. Nautilus now reuses a shared image tree and gives each container its own writable OverlayFS upper directory.

Each cell shows **P50 / P95 in milliseconds**. “Before” is Nautilus `fe26273`. “After” is `b801cc1`. The containerd rows below are the control paired with the optimized runtime. Each row covers 50 measured serial workloads or 48 workloads in 12 batches of four. The raw CSV also includes the warm-ups and the separate containerd measurements paired with the previous runtime. [Raw samples](benchmarks/snapshot-lifecycle-arm64.csv)

| Parallel pods | Runtime | Pod sandbox | Create container | Start container | Create + start | Pod to ready |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | Nautilus before | 7.2 / 8.0 | 66.7 / 68.0 | 31.8 / 35.0 | 98.5 / 101.3 | 110.7 / 113.7 |
| 1 | Nautilus after | 7.8 / 9.4 | 10.8 / 12.6 | 33.1 / 35.7 | 44.1 / 47.2 | 52.4 / 54.7 |
| 1 | containerd 2.2.2 | 68.0 / 75.8 | 13.0 / 16.0 | 48.1 / 54.7 | 62.3 / 71.8 | 129.9 / 142.6 |
| 4 | Nautilus before | 15.6 / 21.4 | 81.4 / 114.8 | 40.8 / 49.4 | 125.5 / 159.9 | 150.0 / 183.3 |
| 4 | Nautilus after | 14.6 / 18.4 | 18.1 / 23.9 | 43.2 / 68.0 | 62.7 / 91.1 | 78.3 / 106.8 |
| 4 | containerd 2.2.2 | 129.6 / 150.2 | 31.6 / 43.0 | 73.8 / 89.4 | 105.0 / 120.2 | 234.3 / 257.9 |

Serial CreateContainer P95 fell by 81.5%. At concurrency four, StartContainer P95 increased from 49.4 to 68.0 ms, while the reduction in create time brought the combined P95 down to 91.1 ms. Full-lifecycle throughput increased from 6.07 to 7.79 workloads/s serially and from 16.43 to 22.86 at concurrency four. The two changes were measured together. The separate measurement windows also showed substantial variation in containerd's concurrent start latency.

### First use of a cached image

The first container pays for unpacking, probing OverlayFS and publishing the reusable tree. With compressed layers already present and the unpacked cache empty, CreateContainer + StartContainer P95 increased from **100.1 to 168.7 ms**. The cached pull stayed inexpensive. This matters for images used only once. With 15 samples per runtime, nearest-rank P95 is the largest observation.

| Runtime | Cached pull | First create | First create + start | Pod to ready |
| --- | ---: | ---: | ---: | ---: |
| Nautilus before | 0.9 / 1.2 | 67.0 / 68.5 | 98.5 / 100.1 | 110.0 / 112.8 |
| Nautilus after | 1.0 / 2.2 | 117.9 / 134.3 | 152.7 / 168.7 | 166.4 / 182.0 |

The first-create driver alternates the previous and optimized runtime for 15 pairs. Before each sample it removes the image record, verifies that the candidate's dedicated unpacked store is empty, and pulls the pinned digest from the local compressed cache. It times the pull separately, then runs the same workload and readiness check as the warm benchmark. Use two preloaded, dedicated Nautilus endpoints and the candidate’s existing snapshot directory.

```sh
sudo env PYTHONPATH="$bench/proto" "$bench/venv/bin/python" scripts/benchmark_first_create.py \
  --before "$BEFORE_SOCK" --after "$AFTER_SOCK" --image "$image" \
  --snapshot-dir "$AFTER_CONTAINER_DIR/.snapshots-v1" --workdir "$bench/probes" \
  --output "$bench/first-create.csv" --pairs 15
```

### Validation and scope

The comparison used release binaries on 4 logical ARM64 CPUs, Linux 6.8, ext4, cgroup v2, rustc 1.98.0 and runc 1.3.4. One isolated systemd manager and D-Bus served the dedicated daemons. Both Nautilus versions used in-process libcontainer. The driver used the measurement boundaries below. Preliminary runs with the previous runtime engine encountered a concurrent start notification error and accumulating D-Bus connections. The tables use the complete reruns with the isolated manager. On the updated daemon the D-Bus connection count was zero between series, and the number of open sockets stayed at four.

Separate CRI checks verified private writes, shared immutable content, image removal while in use, daemon restart with a retained writable layer, deferred eviction, bidirectional mounts, ordinary bundles created by the previous version, and the external runc driver. Assertions also checked workload exit codes, exec output and exec's seccomp filter count. The latency samples cover one small image at concurrency one and four. Larger images, CNI and kubelet scheduling remain separate measurements.

## Original warm-image baseline · 2026-11-24

The table shows **P50 / P95 in milliseconds**. Each runtime completed 50/50 measured serial lifecycles and 48/48 measured lifecycles in 12 batches of four. All readiness, running-state and cleanup checks passed. The [raw samples](benchmarks/cri-lifecycle-arm64.csv) include the excluded warm-up rows.

| Parallel pods | Runtime | Pod sandbox | Create container | Start container | Create + start | Pod to ready |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | Nautilus | 7.2 / 8.0 | 66.0 / 70.7 | 31.5 / 33.9 | 97.5 / 103.0 | 109.5 / 115.0 |
| 1 | containerd | 66.8 / 72.5 | 13.5 / 16.0 | 47.6 / 53.6 | 61.5 / 67.7 | 128.9 / 137.1 |
| 4 | Nautilus | 15.3 / 20.0 | 79.7 / 97.2 | 40.4 / 49.3 | 122.6 / 138.1 | 146.4 / 161.2 |
| 4 | containerd | 127.8 / 154.4 | 30.5 / 42.5 | 78.2 / 106.2 | 110.2 / 136.8 | 238.3 / 285.0 |

Nautilus reached the workload marker sooner at both concurrency levels. Within an existing sandbox, containerd completed CreateContainer and StartContainer sooner. The largest serial P95 gap in Nautilus's favor was sandbox creation (8.0 vs 72.5 ms). Its largest deficit was container creation (70.7 vs 16.0 ms). At that revision Nautilus materialized the full root filesystem for each container.

Full-lifecycle throughput, including status checks and teardown, was 6.32 vs 4.15 workloads/s at concurrency 1 and 17.87 vs 8.46 workloads/s at concurrency 4 (Nautilus vs containerd). Results cover a single run window. The 48 concurrent samples share 12 batches and provide a first look at contention with four workers.

### Configuration for this run

- Nautilus source `e66f409`, locked release build with rustc 1.98.0, in-process libcontainer. Driver revision `eebb2c2`. containerd 2.2.2 used its runc v2 shim, runc 1.3.4, overlayfs and the preloaded pause 3.10.1 image.
- Linux 6.8, ARM64, four logical CPUs, ext4, cgroup v2 with systemd cgroups configured. Both daemons used isolated networks with host-network pods and info-level logging. The workload was the same BusyBox ARM64 image for both. Its config digest was `sha256:b7c873bd97bdc9046fc894e0b216c8aef31e0db4c5b761fb53022e40577ab6c0`.
- Available memory was approximately 14.7 GiB at the start of each series. CPU pressure (`some avg10`) was 0.00% before serial sampling and 2.55% before parallel sampling. The compiler had exited before the first warm-up. The services had separate writable storage on the same filesystem.

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
- **Analysis.** Report completed/attempted counts, nearest-rank P50 and P95 in milliseconds, and the workload and concurrency beside each result. For nearest rank, sort the samples and take item `ceil(p × n)` (one-based). Compare the individual stages alongside end-to-end readiness because the runtimes can perform setup in different RPCs. Retain outliers. A failed RPC, missing marker, unexpected exit or cleanup failure invalidates the batch. Record the failure before repeating.
- **Repeatability.** Repeat in separate time windows before setting a release threshold. Record image digest, runtime revisions and configuration changes for each comparison. The first baseline covers one small image and a warm host filesystem cache.

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
