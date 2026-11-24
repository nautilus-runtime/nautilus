# Nautilus 虚机沙箱的 CRI v1 契约

核对日期为 2026-11-24，Nautilus 源码基线为 `4666bb5`。Kubernetes 最新稳定版为 **v1.37.2**，对应 Go 模块为 **k8s.io/cri-api v0.37.2**，gRPC 包名为 **runtime.v1**。对 Nautilus，最小接入点是一个新的 Pod 级 runtime handler。kubelet 通过现有 RPC 创建沙箱和容器，VM、VMM API、guest agent 以及两者之间的协议都属于运行时内部实现。[稳定版指针](https://dl.k8s.io/release/stable.txt) · [v1.37.2 发布记录](https://github.com/kubernetes/kubernetes/releases/tag/v1.37.2) · [CRI v0.37.2 定义](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto)

## 本地协议与稳定版的精确差异

逐行比较 Kubernetes 发布标签中的 staging 文件、独立 cri-api 模块标签中的文件，以及本地参与 tonic 构建的 proto。两个上游文件字节完全一致。两侧均有 **37 个 RuntimeService RPC、6 个 ImageService RPC、175 个顶层 message、10 个顶层 enum**。完整 diff 只有 `Image` 的两项本地扩展及注释，所有上游 RPC 签名、流方向、类型和字段编号都一致。[Kubernetes staging](https://github.com/kubernetes/kubernetes/blob/v1.37.2/staging/src/k8s.io/cri-api/pkg/apis/runtime/v1/api.proto) · [上游 Image](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L1714-L1740) · [本地 proto](../proto/cri-api/runtime/v1/api.proto#L1737-L1744) · [构建入口](../build.rs)

| 本地独有字段 | 类型及 tag | 用途 |
| --- | --- | --- |
| `Image.nautilus_image_id` | `string 100` | 镜像配置 blob 的 SHA-256，供本地 CLI 显示 |
| `Image.nautilus_created_at` | `int64 101` | 镜像构建时刻，Unix 秒 |

从仓库根目录可复查全部差异。基线中上游文件的 SHA-256 为 `381aab5cf67425b3c90ab016489de661ccb83ed9f21204066dc5df83b9b83360`。上游客户端可忽略这两个未知字段，VM 路由和生命周期均使用标准字段。[Proto3 的未知字段规则](https://protobuf.dev/programming-guides/proto3/#unknowns)

```bash
curl -fsSL https://raw.githubusercontent.com/kubernetes/cri-api/v0.37.2/pkg/apis/runtime/v1/api.proto |
  diff -u - proto/cri-api/runtime/v1/api.proto
```

## Pod、VM 与容器的生命周期

以下按“一次成功的 `RunPodSandbox` 对应一台 VM”设计。同一 Kubernetes Pod UID 可以有多次 sandbox attempt，以 CRI 返回的 sandbox ID 作为 VM 归属标识，保存原始 metadata（含 attempt）、有效 handler 和需要恢复的连接信息。[PodSandboxMetadata](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L565-L575) · [kubelet 的沙箱重建判断](https://github.com/kubernetes/kubernetes/blob/v1.37.2/pkg/kubelet/kuberuntime/util/util.go#L29-L73)

| 操作 | 上游契约 | Nautilus VM 后端的可观察结果 |
| --- | --- | --- |
| 选 handler | kubelet 将 `Pod.spec.runtimeClassName` 解析为 `RuntimeClass.handler`，传入 `RunPodSandbox.runtime_handler`。空串选择默认 handler，拒绝未知名称。 | 例如 `vm` 显式启用 VM，未知名称在分配资源前返回错误。创建容器时按已持久化的 Pod handler 路由。 |
| `RunPodSandbox` | 成功返回时沙箱已经 READY。 | Pod 所需网络、VM 引导、guest agent 握手及创建容器所需能力均已就绪，再返回 sandbox ID。中途失败回收已分配资源，保留可重试清理的信息。 |
| `CreateContainer` / `StartContainer` | 在指定 sandbox 内创建容器。重复传入的 sandbox config 是原配置的引用，操作容器以 container ID 寻址。 | guest 内准备 OCI rootfs、卷和容器配置，先呈现 CREATED，Start 后才运行工作负载。同一沙箱的第二个容器复用 VM。 |
| 状态、列表 | 沙箱仅有 READY / NOTREADY，查询不存在的沙箱或容器返回错误。按请求过滤列表，保留标签、原始注解、创建时间、运行时名称和 IP。 | VM 进程或 guest 失联须能反映到状态，重启 daemon 后以实际存活的 VM 和 guest 容器核对持久记录。容器独立返回状态、退出码及起止时间。 |
| `StopContainer` / `RemoveContainer` | Stop 按超时结束进程，超时后强制终止，Remove 可强制移除运行中的容器。已停止或已删除可重复调用。 | 只影响目标容器，同 Pod 其他容器及 VM 继续运行。 |
| `StopPodSandbox` / `RemovePodSandbox` | Stop 强制终止剩余工作负载并回收网络资源。Remove 强制终止并删除剩余容器。两者均须幂等。 | Stop 与 Remove 均可推进清理。释放 Pod IP、端口、网络连接，结束 VMM，Remove 回收容器状态、卷引用和沙箱记录。重复调用及 daemon 重启后继续清理均可完成。 |

契约来自 [RuntimeService](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L24-L115)、[创建容器请求](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L1390-L1410) 和 [kubelet 的 handler 解析](https://github.com/kubernetes/kubernetes/blob/v1.37.2/pkg/kubelet/kuberuntime/kuberuntime_sandbox.go#L59-L72)。本地 `SandboxBackend.prepare/start` 目前由容器生命周期调用，VM 的创建、重连和回收应由 Pod 层持有。[SandboxBackend](../src/sandbox/mod.rs#L190) · [容器创建](../src/runtime/container.rs#L224) · [Pod 生命周期](../src/runtime/pod.rs#L47)

## RuntimeClass、健康与能力报告

- **节点与单个沙箱。** `Status.status.conditions` 的 `RuntimeReady` 表示运行时具备运行基础容器的能力，`NetworkReady` 表示运行时网络可用。单个 VM 的健康走 `PodSandboxStatus`。条件无法满足时填写 reason 和 message，某个可选 VM 后端故障时，应根据默认运行时和网络的实际能力计算节点状态。[RuntimeCondition](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L1836-L1864)
- **handler 与特性。** `StatusResponse.runtime_handlers` 的名称唯一，空名称代表默认 handler。建议由实际注册且可用的后端生成列表。handler 级标准特性只有递归只读挂载及 user namespaces（含 idmapped mounts）。全局 `RuntimeFeatures` 的 supplemental groups 策略、userns 与 hostNetwork 组合、bind mount options 都应按实际支持填报。需要展示 VMM 类型或版本时可放入 verbose info 的 JSON 值。具有 KVM 的节点可通过 RuntimeClass 的 `scheduling.nodeSelector` 定向调度，声明 handler 本身不会选择节点。[Status 响应](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L1872-L1919) · [RuntimeClass 调度](https://kubernetes.io/docs/concepts/containers/runtime-class/#scheduling)
- **Nautilus 的报告。** `Status` 列出默认 handler（空名称）、`runc` 及已配置的 WASI 别名。显式启用 VM 的节点在真实引导、网络及 guest 工作负载握手通过后增列 `vm`。可选 VM 缺少资产时，普通 OCI 仍可工作。沙箱按请求的 metadata 保存 attempt 并在 Status 和 List 中返回，旧记录缺省为 0。VM 沙箱的 READY 还需要核对实际 VMM 和 guest。[运行时实现](../src/cri/runtime_service.rs) · [真实 kubelet 验收](../tests/kubelet_vm_acceptance.py) · [kubelet 沙箱判断](https://github.com/kubernetes/kubernetes/blob/v1.37.2/pkg/kubelet/kuberuntime/util/util.go#L39-L68)

## 隔离、网络、存储与资源

| 输入 | 运行时应实现或明确拒绝的行为 |
| --- | --- |
| Namespace / 安全上下文 | `POD` 表示同 Pod 共享，`CONTAINER` 仅当前容器，`NODE` 指运行 kubelet 的节点，`TARGET` 指同 Pod 已有的目标容器。Kubernetes 普通 Pod 默认使用每容器独立的 PID 命名空间。VM 内 PID 1 所在空间与宿主 NODE 的含义不同。初版无法兑现 hostNetwork、hostPID、hostIPC、特权、卷传播或指定 profile 等具体请求时，在相应 Pod 或容器创建阶段返回明确错误。传给 guest 的 UID/GID、capabilities、只读根、seccomp 和 mount options 须实际生效。 [NamespaceOption](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L397-L457) · [挂载字段](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L335-L382) |
| Pod 网络 | 同一 Pod 的容器共享 Pod IP、端口和 loopback。实现时可由宿主 CNI 分配网络，通过 TAP 等机制接入 guest，再应用路由、DNS、端口映射，透出有效主 IP 和其他地址。Stop 时释放网络，即使 guest 已死亡也要执行宿主侧回收。测试 Pod 内 localhost、跨 Pod 通信、DNS、hostPort、端口转发及 IP 回收。 [PodSandboxConfig](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L578-L625) · [网络状态](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L671-L718) |
| 镜像与卷 | `ImageService.PullImage` 处理镜像、认证和可能的 `ImageSpec.runtime_handler`。未知非空 handler 应拒绝。guest 内依据容器的 image、命令、环境和 security context 建立独立 rootfs。Pod 共享卷应可见，容器可写层按容器隔离，镜像卷只读。宿主指定的文件、设备和挂载选项须在 guest 对应容器中兑现。 [ImageSpec](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L956-L973) · [ContainerConfig](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L1258-L1313) |
| 资源与开销 | `LinuxPodSandboxConfig.cgroup_parent` 为宿主 Pod cgroup 父级。kubelet 单独计算 `resources` 和 `overhead`，计算前者时排除 overhead，并按已启用的 Pod 级资源等规则聚合。VM 定容需要处理缺省和无限制的值，明确定义 guest 内存、vCPU、宿主 VMM 开销的对应关系。CPU shares 表示权重，quota/period 表示带宽，二者都不能直接当作 vCPU 个数。保留每个容器在 guest 内的 cgroup 限额，并防止 Pod overhead 重复计入。 [LinuxPodSandboxConfig](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L546-L559) · [v1.37.2 kubelet 计算](https://github.com/kubernetes/kubernetes/blob/v1.37.2/pkg/kubelet/kuberuntime/kuberuntime_sandbox_linux.go#L34-L76) |

## 日志、交互及统计

`PodSandboxConfig.log_directory` 与容器相对 `log_path` 定位 kubelet 读取的宿主日志文件。VM 内 stdout、stderr 要以 CRI 日志格式写回，`ReopenContainerLog` 后继续写入轮转后的文件。`ExecSync` 返回两路输出及退出码，timeout 单位是秒，0 表示不设命令超时，按上游约定限制超大输出。`Exec`、`Attach`、`PortForward` 的 gRPC 响应分别给出流式服务 URL。Exec 定位指定 guest 容器，Attach 连接运行中主进程的原有 stdio，TTY 合流并处理窗口大小，PortForward 定位 sandbox 内端口，包括只监听 guest loopback 的服务。[日志与交互 RPC](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L97-L115) · [请求和返回值](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L1614-L1702)

原有宿主 OCI 后端为 Attach 新启 shell，将 PortForward 连接到 Pod IP，在无地址时使用宿主 loopback。VM 的 Attach 接入 guest 原 init 的 stdio，PortForward 在指定 guest 内拨号。工作负载 guest 通过 cgroup v2 提供 VM 容器和 Pod 的 CPU、内存采样。`ContainerStats` 和 `PodSandboxStats` 返回实际用量，两种列表按 ID 和标签筛选可采样对象。旧 agent 会拒绝统计操作，点查通过 CRI 返回 `Unimplemented`。节点上的 VMM cgroup 用于核实整台 VM 的限制。[流式实现](../src/cri/streaming.rs) · [统计实现](../src/cri/runtime_service.rs) · [上游统计类型](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L1973-L2064)

## 高级 RPC 和验证边界

| 功能 | v1.37.2 行为与本项目边界 |
| --- | --- |
| 资源热更新 | `UpdateContainerResources` 要同步、事务性地应用请求。Pod 级 `UpdatePodSandboxResources` 的 proto 注释写为 kubelet 修改 cgroup 后尽力通知，v1.37.2 kubelet 实际先通知运行时，略过 `Unimplemented`，其他错误会阻止此次修改，宿主 cgroup 修改失败还会通知回滚。当前 Nautilus 的两个 RPC 都返回 `Unimplemented`。支持热更新需要分别验证 guest、VMM 和宿主账目，未支持时记录限制。 [proto](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L220-L226) · [实际调用顺序](https://github.com/kubernetes/kubernetes/blob/v1.37.2/pkg/kubelet/kuberuntime/kuberuntime_manager.go#L1002-L1016) · [错误处理](https://github.com/kubernetes/kubernetes/blob/v1.37.2/pkg/kubelet/kuberuntime/kuberuntime_container.go#L432-L453) |
| 批量列表与事件 | 六个 `Stream*` 列表 RPC 发送有限批次，以 EOF 结束，每个对象只出现一次，它们和持续传递生命周期事件的 `GetContainerEvents` 是不同接口。v1.37.2 CRI 客户端遇到列表流 `Unimplemented` 会回退对应的普通列表。当前已提供 Pod、容器、镜像以及 Pod 和容器用量的有限列表流，复用普通列表的过滤及字段语义。每个批次按 Protobuf 编码大小分组，空列表直接结束。Pod 扩展指标流和生命周期事件流仍待实现。 [流式列表语义](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L50-L63) · [运行时回退](https://github.com/kubernetes/kubernetes/blob/v1.37.2/staging/src/k8s.io/cri-client/pkg/remote_runtime.go#L457-L468) · [镜像回退](https://github.com/kubernetes/kubernetes/blob/v1.37.2/staging/src/k8s.io/cri-client/pkg/remote_image.go#L267-L278) |
| Checkpoint / Restore | `CheckpointPod` 携带 deadline 及选定的运行中容器，需在同一冻结窗口捕获，成功、错误或过期均要恢复运行。失败须清理部分文件。`RestorePod` 成功时容器均为 CREATED，恢复的进程要等 `StartContainer` 才执行，失败则清理已建对象。VMM 保存内存只是材料之一，Nautilus 当前对容器 checkpoint、Pod checkpoint 和 restore 均返回 `Unimplemented`。 [RPC 契约](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L161-L184) · [checkpoint / restore 输入](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L2175-L2245) |
| 配置与扩展指标 | `RuntimeConfig` 按节点配置返回 systemd 或 cgroupfs，VM 要求 systemd。kubelet 调用 `UpdateRuntimeConfig` 时，Nautilus 核对非空的 PodCIDR 与节点配置的 CNI 地址分配范围，配置冲突返回 `FailedPrecondition`。扩展指标描述和 Pod 指标 RPC 尚未实现。 [配置契约](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L207-L225) · [本地实现](../src/cri/runtime_service.rs) |

首轮 CRI 验收应在独立端点用指定 VM handler 跑一个 Pod 内两个容器、第二个隔离的 Pod、正常停止、异常退出及删除重试，核对退出码、网络、日志、资源与宿主残留，再通过真实 kubelet + RuntimeClass 重跑同一用户流程。本项目既有 critest v1.31.1 的 96 通过、0 失败、17 跳过，是已有实现的回归基线。VM 实现需分别记录匹配 Kubernetes 1.37 的 cri-tools 套件结果、跳过功能与原因及真实节点验收结果。[既有 critest 记录](CRITEST_RESULTS.md) · [cri-tools v1.37.0](https://github.com/kubernetes-sigs/cri-tools/releases/tag/v1.37.0)
