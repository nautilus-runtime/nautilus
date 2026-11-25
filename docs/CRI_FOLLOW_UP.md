# CRI 和 CLI 后续交付顺序

以 Kubernetes v1.37.2、CRI v0.37.2 和 Nautilus `2e7d6a4` 为基线。优先级依据日常操作、启用后的后台调用和故障影响估计。当前没有部署调用量可用于排序。已交付的事件流、镜像启动和后台自动回收以实测记录为准。[CRI 契约](CRI_VM_CONTRACT.md) · [验证记录](CRITEST_RESULTS.md) · [总任务 #36](https://github.com/nautilus-runtime/nautilus/issues/36)

| 顺序 | 功能 | 使用场景和优先级 |
| --- | --- | --- |
| 1 · P0 | 交互式启动与 Attach | 开发、人工排障和 Agent 工具环境的交互入口。先修复原生 OCI Attach，再让 CLI 复用 OCI 和 VM 的同一调用路径。[#45](https://github.com/nautilus-runtime/nautilus/issues/45) |
| 2 · P0 | CRI 扩展指标 | 已接通原生 cAdvisor 指标和 VM 基础 CPU、内存指标。后续补齐 guest 侧的扩展采样。[#40](https://github.com/nautilus-runtime/nautilus/issues/40) · [PR #47](https://github.com/nautilus-runtime/nautilus/pull/47) |
| 3 · P1 | 原地更新 CPU 和内存 | 在容量调整和部分自动扩缩容流程中触发。先实现原生容器和 kubelet 的 Pod 资源同步，再验证 VM 资源更新。[#41](https://github.com/nautilus-runtime/nautilus/issues/41) |
| 4 · P2 | Checkpoint / Restore | 取证、长任务恢复和迁移时使用。优先交付原生 OCI 的单容器检查点。Kubernetes v1.37 的 Pod 级检查点与恢复是独立的 Alpha 路径。[#42](https://github.com/nautilus-runtime/nautilus/issues/42) · [Kubernetes 发布记录](https://kubernetes.io/blog/2026/08/26/kubernetes-v1-37-release/) |

## P0 交互式启动与 Attach

- **使用方式**。支持 `nautilus run -i` 从管道或终端读取输入，`-t` 分配终端，`-it` 同时启用二者。命令退出后返回实际退出码，继续沿用 `--rm`、`-d` 和隐式 Pod 的生命周期。交互连接断开后，容器按自身的 stdin、`stdin_once` 和终端设置决定后续行为。[Docker run](https://docs.docker.com/reference/cli/docker/container/run/)
- **原生 OCI 先接通 init**。容器后端拥有每次启动生成的 pipe 或 PTY。流式 Attach 按容器 ID 接入原进程，CLI 和 kubelet 共用现有 CRI Attach 地址及流协议。流订阅在进程开始输出前就绪，或用有界缓冲交付首批数据。正常输出继续进入现有 CRI 日志。慢客户端的缓冲有上限，且断连可以释放订阅。无 TTY 时分别交付 stdout 和 stderr。启用 PTY 后交付合并输出，并转发终端大小和终端控制字符。
- **VM 与 WASI**。VM 复用已验证的 guest init stdio 和窗口调整。Kata Containers 4.4.0 也把 stdio、关闭 stdin、窗口调整交给 guest agent。Firecracker v1.18.0 提供 VM 层接口，容器进程的输入输出仍需运行时或 guest 处理。WASI 首版只设计 Preview 1 的 `-i` 管道输入，以可取消的异步流接入现有执行任务。TTY 能力须经独立验证后再支持，当前用清晰的错误报告不支持的组合。[Kata agent 协议](https://github.com/kata-containers/kata-containers/blob/4.4.0/src/libs/protocols/protos/agent.proto) · [Firecracker API](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/src/firecracker/swagger/firecracker.yaml)
- **验收**。在真实 socket 上验证原生 OCI 和 VM 的 `run -it --rm`、`printf ... | nautilus run -i ...`、首条输出、终端窗口变化、Ctrl-C、非零退出码、重复 Attach 仍连接同一 init、断连与清理。用 `kubectl attach` 对同一行为复验。WASI 用读取到 EOF 的模块验证管道输入、停止取消和退出码。后台 `--rm` 回归覆盖已有正常执行和重启路径，并补验 Create 与 Start 之间的中断。

## P0 CRI 扩展指标

- **调用路径**。实现 `ListMetricDescriptors`、`ListPodSandboxMetrics` 和 `StreamPodSandboxMetrics`。启用 `PodAndContainerStatsFromCRI` 的 kubelet 会在 `/metrics/cadvisor` 使用 CRI 扩展指标。基本 Pod 和容器统计沿用已有接口。描述符在 daemon 启动时固定。两种列表共用采样和编码，流使用现有分批逻辑，每个 Pod 出现一次并以 EOF 结束。[kubelet 指标入口](https://github.com/kubernetes/kubernetes/blob/v1.37.2/pkg/kubelet/server/server.go) · [CRI v0.37.2](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto)
- **采样来源**。原生容器先通过归属已经验证的 cgroup v2 读取 CPU、throttling、内存、OOM 和进程数，再核对文件系统、块 I/O 和 Pod 网络。Pod 网络计数从所属 netns 取样一次。VM 使用现有认证 guest 协议扩展可选采样字段，容器用量从 guest 容器 cgroup 读取，Pod 和 VMM 账目保留各自含义。新字段兼容旧 agent；无测量能力的字段不生成数据。WASI 的用量按其能独立归属的计数交付。重复采集限制并发和等待时间，单个不可达 guest 不影响其他 Pod。
- **指标语义**。指标名、类型、标签顺序和量纲对齐 kubelet v1.37.2 使用的 cAdvisor。CRI 载荷使用整数，秒值按实际累计值转换成整数秒。用 `memory.peak` 等字段前核对运行内核和 controller。计数器、量表、缺失值和测得的零分别验证。原生描述符覆盖 critest 要求的指标名，实测覆盖面见验收记录。[cAdvisor 定义](https://github.com/google/cadvisor/blob/lib/v0.60.5/lib/metrics/prometheus.go) · [cri-tools 断言](https://github.com/kubernetes-sigs/cri-tools/blob/v1.37.0/pkg/validate/pod.go) · [验证记录](CRITEST_RESULTS.md)
- **验收**。真实 kubelet 的 `/metrics/cadvisor` 返回符合定义的 CRI 指标。制造 CPU、内存、磁盘和网络负载，比较宿主 OCI、VM 容器、Pod 和空闲邻居的实际测量值。核对累计量、标签、独立 CLI 工作负载可见范围、旧 agent 和不可达 guest。列表和流结果在同一采样口径下等价，完整 critest 的指标用例通过后合入该功能。

## P1 原地更新资源

- **原生容器先交付**。接入 `UpdateContainerResources`，首批支持 Linux CPU 权重、配额、周期和内存上限。复用创建规格和当前 cgroup 驱动的字段语义，预先校验完整请求。按 Pod 操作锁串行化更新与停删，从所属 cgroup 读取旧值，应用并回读后才提交资源记录和成功响应。部分写入失败时恢复旧配置并验证；回滚失败时保留待对账状态。内存降低必须验证失败和回滚行为，也记录回收或 OOM 的可能影响。[Kubernetes in-place resize](https://kubernetes.io/docs/tasks/configure-pod-container/resize-container-resources/)
- **Pod 同步与 VM**。接入 `UpdatePodSandboxResources` 时遵循实测 kubelet v1.37.2 调用顺序，包括第二次调用回滚。Kubelet 仍管理其拥有的宿主 Pod cgroup。VM 内单容器变更通过自研 agent 写入目标 guest cgroup，按 Pod 现有容量校验；宿主 Pod、VMM 和 guest 的外层配额及容量分开评估。Kata 4.4.0 提供 guest 容器更新接口。Firecracker v1.18.0 的 balloon 是运行中的内存回收机制，Cloud Hypervisor v55.0 则提供 VM resize。先验证现有容量内的配额更新，再单独验收虚拟 CPU 或 RAM 容量变化。无法安全完成的方向或配置返回明确错误。WASI 的资源限制需先验证 Wasmtime 按实例执行与统计能力。[现行 kubelet 调用顺序](https://github.com/kubernetes/kubernetes/blob/v1.37.2/pkg/kubelet/kuberuntime/kuberuntime_manager.go) · [Kata agent](https://github.com/kata-containers/kata-containers/blob/4.4.0/src/libs/protocols/protos/agent.proto) · [Firecracker API](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/src/firecracker/swagger/firecracker.yaml) · [Cloud Hypervisor v55 API](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v55.0/docs/api.md)
- **验收**。从真实 kubelet 发出 resize，核对容器 PID、Pod UID 和 VM boot ID 以及应用连续性。检查前后 cgroup 配额、上限、CRI 状态与 kubelet 观测。注入部分应用失败、kubelet 回滚通知和 daemon 重启。VM 另查 guest、VMM 和宿主三层限制，以及同 Pod 其他容器与其他 Pod 的行为。

## P2 Checkpoint / Restore

- **先交付原生单容器**。在通过节点能力检查的 runc／CRIU 组合上接入 `CheckpointContainer`。通过标准 kubelet checkpoint 接口产生可检验的归档，记录镜像身份、平台、运行时格式和兼容性信息。限制归档访问权限，并在失败或超时后恢复执行和清理部分输出。[kubelet Checkpoint API](https://kubernetes.io/docs/reference/node/kubelet-checkpoint-api/)
- **再交付原生 Pod**。在验证所选容器、调用截止时间和目录归属后，暂停全部所选容器，再捕获同一冻结窗口。操作由 daemon 持有，成功、失败和取消路径均恢复源工作负载。`RestorePod` 预备沙箱和新容器 ID，所有目标保持 CREATED，原进程等到各自的 `StartContainer` 再运行。恢复失败回收新建资源。首版选择同主机、同架构、经过能力核验的场景，跨节点及外部卷恢复单独验收。[CRI v0.37.2](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto)
- **VM 要先确认一致性**。Kata 4.4.0 的 guest 协议提供容器级 Pause / Resume，可作为工作负载协作冻结的参考。Firecracker v1.18.0 把 VM 内存、设备状态和由调用方管理的磁盘分开处理。Nautilus VM 优先试验 guest 内容器检查点，在通过隔离验收后接入 CRI。整机快照需另外证明 Cloud Hypervisor、virtiofsd、可写根文件系统、挂载卷与网络恢复一致，再决定开放范围。WASI 先验证可恢复执行状态的能力再承诺支持。[Kata agent](https://github.com/kata-containers/kata-containers/blob/4.4.0/src/libs/protocols/protos/agent.proto) · [Firecracker 快照](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/docs/snapshotting/snapshot-support.md) · [Cloud Hypervisor v55 快照](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v55.0/docs/snapshot_restore.md)
- **验收**。用两个容器内可观察的进度核对冻结窗口和恢复顺序。Pod UID、日志目录和 cgroup 都使用目标请求的语义。模拟 deadline、无效检查点、挂载不兼容和中途失败，核对源 Pod 可继续、目标未提前执行、失败资源归零。

## 实施与合入

交互首个 PR 交付原生 OCI init Attach。指标从 #47 的采样实现拆分原生 cgroup 与进程、存储与网络、guest 三段，与交互并行开发。前两段资源更新按原生容器与 Pod／VM 拆开。检查点按单容器、Pod、VM 能力验证拆开。Issue 与 PR 使用英文，逐项记录功能范围和验收证据。

所有 CRI 行为变更按仓库规则在 Linux 跑 Rust 检查与未过滤的 `critest`，Runtime 和 Image 使用被测 daemon 的 socket。Kubernetes 功能用真实 kubelet 验收。VM 记录 VMM、host kernel、guest 和清理结果。继续复用独立的 pro6000 测试节点。[仓库验收规则](../AGENTS.md) · [验证记录](CRITEST_RESULTS.md)
