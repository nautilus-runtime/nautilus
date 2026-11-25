# CPU agent 在 Pod VM 中执行工具

本轮使用 `pro6000` 的独立嵌套 KVM 节点。每个会话通过 Nautilus 的 CRI socket 创建一个新的 `vm` PodSandbox、一套独立 rootfs 和一个工具容器，连续工具调用沿用同一容器的文件系统。宿主控制器通过模型的实际回答选择工具，工具在 guest 中读取测试结果、修改配置并重新测试。[基准程序](../tests/vm_agent_loop_benchmark.py) [agent 与工具定义](../tests/vm_agent_loop_workload.py)

## 测试环境

| 项目 | 配置 |
|---|---|
| 物理机 | AMD Ryzen Threadripper 9970X，32 核 64 线程，188 GiB RAM，Linux 5.15.0-191-generic |
| 运行 Nautilus 的独立节点 | Debian 13，Linux 6.12.118+deb13-amd64，12 vCPU，内核报告 11.7 GiB 总内存，45 GiB virtio 磁盘，ext4 |
| daemon | Nautilus `12e7d5c`，Linux x86_64，`cargo build --locked --release --bin nautilusd`，运行中进程的 SHA-256 为 `25d16af1eae45fdee0633128136881933a1ac967e9bc8140d8e9338a6d8896a1` |
| VM 和工具 | Cloud Hypervisor v55.0，virtiofsd 1.13.2，guest Linux 5.15.0-191-generic，runc 1.5.3，crictl v1.37.0 |
| 每个会话 | 1 vCPU，384 MiB guest RAM，额外 128 MiB 开销。VMM 的 `cpu.max` 为 `100000 100000`，`memory.max` 为 512 MiB |
| OCI 工具镜像 | 21 层实际 Pi harness，manifest `sha256:2b02dca554ac764e1264ac242cebebbf6eecf6fbe64c503383b6bc486f7ec453`，镜像配置 `sha256:b78c1fd9096918f80cf62e10d045df613b2b7513ff5e36b3bf6e43d2e07cb8a0` |

源镜像和控制面的临时状态均位于独立节点。约 257 MiB 的压缩层已按内容摘要预置。在新的存储目录中，第一次 CRI PullImage 解包耗时 4.395 秒，立即对同一摘要再次 PullImage 耗时 11.631 毫秒，各测一次。节点的页缓存沿用前面的测试。后续每次采样都新建并最终删除一个 Pod VM。独立拉取公开 BusyBox 的单次耗时为 7.263 秒。

模型由物理机上的 llama.cpp b12676（`d35dc4d9e`）以 CPU 模式运行。Qwen2.5-1.5B-Instruct-GGUF 使用 revision `91cad51170dc346986eccefdc2dd33a9da36ead9` 的 Q4_K_M 权重，SHA-256 为 `6a1a2eb6d15622bf3c96857206351ba97e1af16c30d7a74ee38970e434e9407e`。服务参数为 `-ngl 0 -t 4 -tb 4 -np 4 -c 32768 --jinja --alias nautilus-bench`。控制器在 Nautilus 节点发起推理请求，模型读到实际 guest 工具的输出，并在四个并行推理槽上做下一步选择。每个会话随机要求升序、降序或排序去重。首步固定为运行失败的测试，模型从尚未试过的修复中选择，编辑后再次测试；失败的修复从下一轮候选中剔除。成功率表示在给定工具集内完成一次会话，首次修复命中率单独统计。[模型版本](https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/tree/91cad51170dc346986eccefdc2dd33a9da36ead9) [llama.cpp 版本](https://github.com/ggml-org/llama.cpp/releases/tag/b12676)

## 测量方法

- 冷启动从 `crictl runp` 前开始，经过 CreateContainer、StartContainer，直至首条 ExecSync 返回合法的 guest boot ID。这个口径使用已拉取的镜像，包含每次建容器准备独立 rootfs 的时间。每个并发波次校验 sandbox ID、容器 ID、Pod IP 和 boot ID 各不相同。现有 VM 后端对每个 CreateContainer 调用 `materialize_rootfs`，从缓存的镜像层解包到该容器的独立目录。[VM 容器创建](../src/sandbox/vm_backend.rs) [镜像物化](../src/storage/pull.rs)
- 热调用在同一容器内顺序读上次写入的值，再写入本次结果。每个耗时包含启动本地 crictl、CRI、vsock、guest runc 和工具进程，以及将输出读回客户端。首轮要求状态文件尚不存在，之后每轮必须读取并核对上一轮的值。所有 P50 和 P95 使用样本排序后的最近秩。同一波的每个会话顺序做 10 次调用，会话之间并行。聚合吞吐量由每波总调用数除以该波热调用阶段墙钟时间得出。
- 会话启动并完成该波安排的模型工作后，空闲 1 秒，再按 VMM 和 virtiofsd 各自的 systemd ControlGroup 枚举进程，从 `/proc/PID/smaps_rollup` 读取并汇总 PSS。每项原始记录含对应 PID、cgroup `memory.current` 和约束值，PSS 单位换算为 MiB。它衡量运行 Pod VM 的节点上这两项服务的实际驻留物理内存份额，节点上的 daemon 和镜像文件缓存另计。
- 取消通过主动关闭一条已在 guest 启动子进程的 ExecSync 客户端触发。记录子进程的 Linux 状态和同一 guest 继续执行的结果，再由 CRI 停止和删除会话。每波结束都对照测试前的 VMM/virtiofsd 服务、owner、网络记录、netns、IPAM 租约，以及节点 mountinfo 中配置的运行、容器、存储、临时和网络目录内的挂载。结束时还按运行标签查验剩余 CRI 容器和 sandbox 数。

## Release 实测

共运行 32 波、110 个新 Pod 与 1,100 次热 ExecSync。每一行给出该并发下会话耗时与同一会话空闲 PSS 的 P50 / P95，吞吐列为每波聚合吞吐的中位数。

| 并发会话 | 波数 / Pod 数 / 热调用数 | 冷至首个 exec（秒） | 热 exec（毫秒） | 每个 VM 的 VMM + virtiofsd PSS（MiB） | 热调用/秒 |
|---:|---:|---:|---:|---:|---:|
| 1 | 20 / 20 / 200 | 5.724 / 6.066 | 39.85 / 67.37 | 164.38 / 167.25 | 25.6 |
| 2 | 3 / 6 / 60 | 6.436 / 6.529 | 31.79 / 56.51 | 161.56 / 162.71 | 60.7 |
| 4 | 3 / 12 / 120 | 7.409 / 7.730 | 27.77 / 50.38 | 159.66 / 161.77 | 124.0 |
| 8 | 3 / 24 / 240 | 9.954 / 10.152 | 31.40 / 66.34 | 158.75 / 160.76 | 212.2 |
| 16 | 3 / 48 / 480 | 19.464 / 20.504 | 52.72 / 83.29 | 158.27 / 160.26 | 271.6 |

单会话下，RunPodSandbox、CreateContainer、StartContainer、首个 ExecSync 的 P50 分别为 1.337、4.306、0.019、0.044 秒。另用同一提交的开发构建测了 20 个单会话，冷启动 P50 / P95 为 17.234 / 17.372 秒，200 次热 exec 为 33.55 / 61.56 毫秒。这组对照单独采集。

模型波次使用同一组 Pod 中的会话，按各自的创建起点计时。首条模型工具包含创建 VM、容器、校验 boot ID、准备工作区、模型端等待与执行首个 guest 测试的时间。agent 总耗时从容器就绪后准备工作区起算。每次模型请求及其选中的 guest 工具各算一个样本。

| 并发会话 | 成功 / 失败 | 首次修复正确 | 工具轮数 | 冷至首条模型工具（秒） | agent 总耗时（秒） | 模型单轮（毫秒） | 对应 guest 工具（毫秒） |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 7 / 0 | 1/7 | 43 | 5.945 / 6.252 | 2.023 / 2.097 | 210.25 / 275.14 | 55.57 / 84.68 |
| 2 | 2 / 0 | 1/2 | 8 | 6.622 / 7.071 | 1.242 / 1.781 | 280.47 / 429.15 | 64.05 / 75.25 |
| 4 | 4 / 0 | 2/4 | 16 | 7.832 / 8.533 | 2.085 / 3.016 | 454.39 / 893.09 | 62.87 / 83.51 |
| 8 | 8 / 0 | 0/8 | 46 | 10.763 / 11.397 | 8.069 / 9.434 | 1437.78 / 2299.07 | 56.04 / 88.92 |
| 16 | 16 / 0 | 2/16 | 94 | 20.481 / 22.165 | 12.203 / 13.067 | 1619.36 / 2630.98 | 63.58 / 90.07 |

37 个会话共完成 207 轮推理和工具调用，0 个会话运行失败，6 个会话首次修复正确。三个随机目标各抽到升序 6、降序 14、去重 17 次。每个会话用了 3 至 7 轮。模型端有 4 个并行槽，16 个并行会话包含排队等待。并发 2 和 4 的模型样本分别只有 2 和 4 个，这两行的 P95 即观测最大值。

## 容量与回收

16 并发时，16 个 VMM 和配套 virtiofsd 合计 PSS 的三波中位数为 2,525.85 MiB，实测每个 VM 的 VMM 配额均为 1 核、512 MiB。运行节点有 12 个 vCPU。并发从 8 增到 16 时，热调用中位吞吐从 212.2 增到 271.6 次/秒，冷启动 P95 从 10.152 增到 20.504 秒。16 是此次实测的最高并发，工作负载只使用少量 guest 内存。每 20 秒采样一次的节点可用内存最低为 7.24 GiB，磁盘可用空间最低为 32.67 GiB。

同一个 daemon 全程未重启。它在各档开始前的 PSS 分别为 81、86、284、347、763 MiB。全部容量波次与后续单次 Pi 验收结束后，确认 CRI 中 Pod 和容器各为 0 时，daemon PSS 为 1.15 GiB。上述每 VM 的 PSS 只计 VMM 与 virtiofsd，做节点预算时还需单独计入 daemon。随后用同规模 OCI 镜像复现并修复了这类解包驻留，见下方复验。

五个档位各取消一条正在执行的工具命令。49.53–64.82 毫秒内检测到同一 PID 与启动时间的 sleep 已退出，并成功在原 guest 再执行命令。测试容器以长期 sleep 作 PID 1，五例的子进程状态均为 `Z`，直至移除该容器的 PID 命名空间才释放。其他 0、1、3、7、15 个并行会话均保留原 boot ID 与先前写入的工作区内容。32 波均清除本波的 VM 与 virtiofsd 服务、owner 记录、范围内挂载、网络命名空间及 IP 租约。最终剩余带测试标签的 workload 和 sandbox 均为 0。单会话与 16 并发整波 Stop/Remove 的 P50 分别为 1.130 和 4.143 秒。

### daemon 解包内存复验

在 pro6000 的独立 Debian 13 节点上，用约 250 MiB、21 个压缩层的测试镜像，经真实 CRI 依次运行 4、8、16 个并发 Pod VM，每波各运行两条热调用并完成清理。对比 Release 版 `40dc126` 与修复版 `b5f9af9`，两轮均使用默认 glibc 分配器配置。

| daemon | 测试开始 PSS | 清理 28 个 Pod 后 PSS | 清理后匿名 PSS |
| --- | ---: | ---: | ---: |
| 原整层读取 | 62.0 MiB | 447.4 MiB | 433.3 MiB |
| 流式读取 | 31.9 MiB | 62.9 MiB | 48.8 MiB |

原版的阻塞线程退出后，PSS 仍约 447 MiB。独立的分配器诊断轮在清理后观察到约 293 MiB 的空闲 arena 内存和约 39 MiB 的在用 arena 内存。只固定 glibc 的 mmap 阈值再跑同一组负载，原版清理后降到 69.0 MiB。解包此前为每个层分配文件大小的堆缓冲。修复后从同一个已打开的缓存文件分段读取并回卷，保留先处理 whiteout、再应用层的顺序。[层解包](../src/storage/unpack.rs) · [glibc 分配器参数](https://sourceware.org/glibc/manual/latest/html_node/Memory-Allocation-Tunables.html) · [跟踪任务 #63](https://github.com/nautilus-runtime/nautilus/issues/63)

基准报告记录清理后的 daemon PSS 与增长量。给 `--daemon-unit` 同时传入 `--max-daemon-pss-growth-mib` 可启用回归预算。单波 8 并发用同一镜像和 96 MiB 预算验证，旧版增长 206.8 MiB，修复版增长 33.1 MiB。此数据来自 OCI 层与 CRI 生命周期对照，容量上限和长稳验证仍待单独测量。[回归基准](../tests/vm_agent_loop_benchmark.py)

进一步在同一镜像上重复原有规模，两版各完成 32 波、110 个 Pod 和 1,100 次热 ExecSync。一次旧版的 16 路同时清理出现长时间阻塞，经重启测试 daemon 后回收了残留 Pod。完成的配对复验仍以最多 16 路并发启动，将两版的清理并发统一为 4。清场后 daemon PSS 从旧版的 705.0 MiB 降到修复版的 71.9 MiB，两轮均无残留 Pod 或容器。

## 镜像内的 Pi CLI

同一个 21 层 harness 镜像包含 Node.js v22.22.0 和 Pi v1.5.2。镜像内脚本在首次会话让 Pi 使用 `read` 读取刚生成的随机标识，再用同一会话的历史让 Pi 使用 `write` 将标识写入另一文件。脚本核对两个文件的内容、两次工具成功事件与两轮最终 assistant 状态。Release 构建从创建 PodSandbox 到观察到 Pi 容器以退出码 0 结束耗时 11.45 秒。两轮均收到了成功的最终 assistant 状态，并读写了正确的随机值。开发构建上先前的独立实测为 20.87 秒，两种构建各测一次。[Pi 会话脚本](../tests/vm_agent_loop_pi.sh)

把脚本放入镜像或通过只读卷挂入后，在 Pod 内执行。此轮测试的模型从 guest 的 CNI 网关 `10.233.30.1:42313` 提供，复现时传入自己 Pod 可达的模型地址。

```sh
export MODEL_URL="http://10.233.30.1:42313/v1"
export PI_BIN=/opt/teamster/internal/runtime/harness/pi/node/node_modules/.bin/pi
sh tests/vm_agent_loop_pi.sh
```

## 复现

在启用了 Pod VM 的专用 Linux 节点执行。需要有读取 `/proc/PID/smaps_rollup`、systemd 和 CNI 私有目录的权限，Python 3.11+，以及能连接测试 daemon 的 crictl。将 `IMAGE` 设为已拉取的实际工具镜像摘要。可公开拉取的替代镜像为 `registry.k8s.io/e2e-test-images/busybox@sha256:f1fa97a4e1dab565da2790e6c76f8aecf87c44c9ac7ba53a9c65af96ec472c4a`。若改用该镜像或其他含 POSIX shell、cat、sort、cmp、mktemp 与 sleep 的工具镜像，记录实际摘要，以便解释 rootfs 准备时间。[节点配置](VM_RUNTIMECLASS.md)

```sh
for level in 1 2 4 8 16; do
  model_waves=1
  if [ "$level" = 1 ]; then model_waves=7; fi
  sudo python3 tests/vm_agent_loop_benchmark.py \
    --config /etc/nautilus/nautilus.toml --ipam-dir /var/lib/cni/networks/nautilus \
    --daemon-unit nautilus.service --image "$IMAGE" --report "capacity-$level.jsonl" \
    --levels "$level" --singles 20 --waves 3 --execs 10 \
    --model-url "$MODEL_URL" --model nautilus-bench --agent-waves "$model_waves"
  python3 tests/vm_agent_loop_benchmark.py --summarize --report "capacity-$level.jsonl"
done
```

将上面的 unit、配置与 IPAM 目录替换为测试节点的实际值。模型端点提供兼容的 `/v1/chat/completions` 与 JSON Schema 响应约束，`MODEL_URL` 以 `/v1` 结尾。省略 `--model-url` 可仅测试 CRI 生命周期、热调用和驻留内存。每个会话在自己的容器启动后立即进入 agent。原始 JSON Lines 按波记录推理和 guest 工具的耗时、实际动作、冷启动分段和 PSS。汇总要求预期波数、逐个会话样本、回收成功与最终完成记录全部齐全，若有模型会话失败则返回非零状态。
