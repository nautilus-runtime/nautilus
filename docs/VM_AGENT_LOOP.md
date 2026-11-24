# CPU agent 在 Pod VM 中执行工具

本轮使用 `pro6000` 的独立嵌套 KVM 节点。每个会话通过 Nautilus 的 CRI socket 创建一个新的 `vm` PodSandbox、一套独立 rootfs 和一个工具容器，连续工具调用沿用同一容器的文件系统。宿主控制器通过模型的实际回答选择工具，工具在 guest 中读取测试结果、修改配置并重新测试。[基准程序](../tests/vm_agent_loop_benchmark.py) [agent 与工具定义](../tests/vm_agent_loop_workload.py)

## 测试环境

| 项目 | 配置 |
|---|---|
| 物理机 | AMD Ryzen Threadripper 9970X，32 核 64 线程，188 GiB RAM，Linux 5.15.0-191-generic |
| 运行 Nautilus 的独立节点 | Debian 13，Linux 6.12.118+deb13-amd64，12 vCPU，内核报告 11.7 GiB 总内存，45 GiB virtio 磁盘，ext4 |
| daemon | Nautilus `12e7d5c`，Linux x86_64 开发构建，SHA-256 `430fb4e1c03b08fe0dcd41d02d64c2e152bc9bc3afd1061b76a4bdc4eb700210` |
| VM 和工具 | Cloud Hypervisor v55.0，virtiofsd 1.13.2，guest Linux 5.15.0-191-generic，runc 1.5.3，crictl v1.37.0 |
| 每个会话 | 1 vCPU，384 MiB guest RAM，额外 128 MiB 开销。VMM 的 `cpu.max` 为 `100000 100000`，`memory.max` 为 512 MiB |
| OCI 工具镜像 | 21 层实际 Pi harness，manifest `sha256:2b02dca554ac764e1264ac242cebebbf6eecf6fbe64c503383b6bc486f7ec453`，镜像配置 `sha256:b78c1fd9096918f80cf62e10d045df613b2b7513ff5e36b3bf6e43d2e07cb8a0` |

源镜像和控制面的临时状态均位于独立节点。约 257 MiB 的压缩层已按内容摘要预置。第一次 CRI PullImage 解包耗时 16.090 秒，立即对同一摘要再次 PullImage 耗时 23.942 毫秒，各测一次。后续每次采样都新建并最终删除一个 Pod VM。独立拉取公开 BusyBox 的单次耗时为 17.392 秒。

模型由物理机上的 llama.cpp b12676（`d35dc4d9e`）以 CPU 模式运行。Qwen2.5-1.5B-Instruct-GGUF 使用 revision `91cad51170dc346986eccefdc2dd33a9da36ead9` 的 Q4_K_M 权重，SHA-256 为 `6a1a2eb6d15622bf3c96857206351ba97e1af16c30d7a74ee38970e434e9407e`。服务参数为 `-ngl 0 -t 4 -tb 4 -np 4 -c 32768 --jinja --alias nautilus-bench`。控制器在 Nautilus 节点发起推理请求，模型读到实际 guest 工具的输出，并在四个并行推理槽上做下一步选择。[模型版本](https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/tree/91cad51170dc346986eccefdc2dd33a9da36ead9) [llama.cpp 版本](https://github.com/ggml-org/llama.cpp/releases/tag/b12676)

## 测量方法

- 冷启动从 `crictl runp` 前开始，经过 CreateContainer、StartContainer，直至首条 ExecSync 返回合法的 guest boot ID。这个口径使用已拉取的镜像，包含每次建容器准备独立 rootfs 的时间。每个并发波次校验 sandbox ID、容器 ID、Pod IP 和 boot ID 各不相同。
- 热调用在同一容器内顺序读上次写入的值，再写入本次结果。每个耗时包含启动本地 crictl、CRI、vsock、guest runc 和工具进程，以及将输出读回客户端。每次调用均校验输出。所有 P50 和 P95 使用样本排序后的最近秩。
- 空闲时按 VMM 和 virtiofsd 各自的 systemd ControlGroup 枚举进程，从 `/proc/PID/smaps_rollup` 读取并汇总 PSS。每项原始记录含对应 PID、cgroup `memory.current` 和约束值，PSS 单位换算为 MiB。它衡量运行 Pod VM 的节点上这两项服务的实际驻留物理内存份额，节点上的 daemon 和镜像文件缓存另计。
- 取消通过主动关闭一条已在 guest 启动子进程的 ExecSync 客户端触发。记录子进程的 Linux 状态和同一 guest 继续执行的结果，再由 CRI 停止和删除会话。每波结束都对照测试前的 VMM/virtiofsd 服务、owner、网络记录、netns 和 IPAM 租约。结束时还按运行标签查验剩余 CRI 容器和 sandbox 数。

## 复现

在启用了 Pod VM 的专用 Linux 节点执行。需要有读取 `/proc/PID/smaps_rollup`、systemd 和 CNI 私有目录的权限，Python 3.11+，以及能连接测试 daemon 的 crictl。将 `IMAGE` 设为已拉取的实际工具镜像摘要。可公开拉取的替代镜像为 `registry.k8s.io/e2e-test-images/busybox@sha256:f1fa97a4e1dab565da2790e6c76f8aecf87c44c9ac7ba53a9c65af96ec472c4a`。若改用该镜像或其他含 POSIX shell、cat、sort、cmp、mktemp 与 sleep 的工具镜像，记录实际摘要，以便解释 rootfs 准备时间。[节点配置](VM_RUNTIMECLASS.md)

```sh
python3 tests/vm_agent_loop_benchmark.py \
  --config /etc/nautilus/nautilus.toml --ipam-dir /var/lib/cni/networks/nautilus \
  --image "$IMAGE" --report capacity.jsonl \
  --levels 1 2 4 8 --singles 20 --waves 3 --execs 10

python3 tests/vm_agent_loop_benchmark.py --summarize --report capacity.jsonl
```

如需同时运行模型选择的工具调用，在上述命令中指定 `--model-url "$MODEL_URL" --model "$MODEL" --agent-waves 1`。端点提供兼容的 `/v1/chat/completions` 与 JSON Schema 响应约束，传入的 URL 以 `/v1` 结尾。每个会话在自己的容器启动后立即进入 agent。原始 JSON Lines 含每次模型推理和 guest 工具的耗时、实际选择的动作、冷启动分段、PSS 和回收结果。
