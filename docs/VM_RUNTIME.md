# Pod 级虚机运行时

设计跟踪于 [#1](https://github.com/nautilus-runtime/nautilus/issues/1)。目标是在 Kubernetes 中为工作负载选择 `RuntimeClass.handler: vm`，让一次 PodSandbox attempt 拥有一台 Linux VM。同一沙箱内的容器在该 guest 中运行，重复 exec 和工具调用沿用原容器的工作区。当前实现提供节点探针。面向 kubelet 的 handler 在完成容器、网络与恢复验收后启用。[CRI v1 契约与本地差异](CRI_VM_CONTRACT.md)

## 所有权与协议

| 边界 | 职责 |
| --- | --- |
| kubelet → nautilusd | 使用标准 CRI v1。以 sandbox ID 寻址每次 attempt，以 container ID 寻址容器，保持默认 OCI/WASI 路由。 |
| nautilusd → Cloud Hypervisor | 每个 sandbox 拥有独立的 VMM 进程、控制 socket、vsock 端点和可写存储。VMM 引导、停止、资源配置与恢复由宿主管理。 |
| nautilusd → Nautilus guest agent | 通过 vsock 进行协议版本和能力协商。团队选择自研 guest agent，负责 guest 内的 OCI 容器、进程、挂载、日志及 exec，可在 guest 内复用 runc。Kata 的 Pod/shim 分工可用作参考。 |
| guest → 工作负载 | 不同容器持有独立 rootfs 和容器级资源约束，按 CRI 的 namespace、卷和 Pod 网络语义共享相应资源。工作负载无需访问 VMM 控制 socket。 |

生产协议按 sandbox、container 和请求 ID 寻址。对创建与删除等可重试操作保存结果；流式操作区分 stdout、stderr、结束状态和取消。guest 启动时校验宿主协商的协议与能力。设计需覆盖消息长度、请求超时、重连、并发以及消息与目标 VM 的绑定。节点探针的单次固定握手只用于检查宿主、内核和传输链路。[Kata Pod 模型](https://github.com/kata-containers/kata-containers/blob/4.4.0/docs/design/architecture_4.0/architecture.md) · [Cloud Hypervisor vsock](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v55.0/docs/vsock.md)

## 创建、恢复与删除

1. `RunPodSandbox` 先解析 handler、策略和所请求的 namespace、资源、网络配置。以原子方式登记 Pending attempt，再分配该 attempt 的资源。拒绝无法满足的请求，禁止回退到宿主容器执行。
2. 建立网络与 VM，等待 guest agent 握手，确认能够创建工作负载、Pod 所需的网络已经可用，再持久化 READY 并返回 ID。guest 启动失败或 CRI 取消时按已完成步骤反向回收。
3. `CreateContainer` 在同一 VM 内准备 OCI rootfs 和配置。`StartContainer` 才启动容器进程。`ExecSync`、streaming、状态和日志操作均定位该 guest 中的目标容器。容器停止后保存真实退出码、时间和日志。
4. daemon 重启时，用持久化的 sandbox ID、受管理的私有 socket、进程身份及 guest 标识重新确认所有权。仅凭 PID 可能命中已被复用的进程。身份不匹配或 guest 失联时报告 NOTREADY，并保留能够安全回收的记录。
5. `StopPodSandbox` 终止工作负载并释放网络；`RemovePodSandbox` 还要移除剩余容器及 VM 存储。以 sandbox 为粒度串行化状态变更。清理途中失败时保留重试信息，资源释放完毕后才删除持久化记录。

## 节点探针

本地执行 `nautilus vm probe` 会启动一台使用 1 个 vCPU、384 MiB 内存的临时 VM，经 guest 端口 4081 交换 `PING 1` 和 `PONG 1`，然后回收子进程和 socket。它可以在 nautilusd 启动之前运行。测试 guest 仅应答就绪请求，无网络、持久磁盘或执行任意命令的接口。

在 x86_64 Linux 验证机上准备已校验的 Cloud Hypervisor 可执行文件、可读的 guest 内核，以及与该内核版本匹配的模块。示例脚本使用静态 C 工具链、kmod、GNU cpio 和 gzip；压缩模块需要对应的解压工具。运行探针的账号需要能够读写 `/dev/kvm`。

```sh
./examples/vm-probe/build-initramfs.sh /tmp/nautilus-probe.cpio.gz "$(uname -r)"
nautilus vm probe --vmm /opt/cloud-hypervisor/bin/cloud-hypervisor \
  --kernel "/boot/vmlinuz-$(uname -r)" --initramfs /tmp/nautilus-probe.cpio.gz
```

主线使用 Cloud Hypervisor **v55.0**，其最低宿主 Linux 版本为 6.8，x86 要求 KVM 暴露 XSAVE2。Linux 5.15 机器使用 **v54.1** 作兼容性验证。运行探针时可显式传入另一个版本的 VMM 与 guest 镜像。后续发布记录分别列出宿主内核、VMM、guest 镜像和实际覆盖的 CRI 路径。[v55.0 宿主条件](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v55.0/README.md#host-os) · [v54.1 宿主条件](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v54.1/README.md#host-os)

2026-11-24 用完整的 Linux x86_64 CLI 和本仓库构造的 initramfs 做了两次 KVM 验证。原生宿主 Linux 5.15.0-191 + VMM v54.1 返回 `PONG 1`；同机启动的临时 Debian Linux 6.12.118 嵌套 KVM 宿主 + VMM v55.0 也返回 `PONG 1`。两次探测的目标 guest 均使用 Linux 5.15.0-191。无效 VMM 可用 `--vmm /bin/false` 验证：命令应快速报告 VMM 已退出，返回非零状态。错误应答与未应答的回归测试可用 `cargo test --locked --lib vm::tests` 运行。

## 交付顺序

| 事项 | 依赖 | 合并前验收 |
| --- | --- | --- |
| [#2 节点与 guest 握手](https://github.com/nautilus-runtime/nautilus/issues/2) | 可独立实施 | 真实 KVM 上的 CLI 验证就绪协议；错误应答、超时和 VMM 提前退出均被识别并清理。 |
| [#3 每个 Pod 持有一台 VM](https://github.com/nautilus-runtime/nautilus/issues/3) | #2 | 两个 sandbox 各自拥有 VM；验证 daemon 重启、启动中断、并发停止/删除、重复调用与身份校验。 |
| [#4 guest 内 OCI 容器](https://github.com/nautilus-runtime/nautilus/issues/4) | #3 | 同一 VM 两个 OCI 容器分别创建、启动、执行与停止；十次调用保持工作区并正确返回退出码和日志。 |
| [#5 Pod 网络与资源](https://github.com/nautilus-runtime/nautilus/issues/5) | #3 | CNI、TAP、Pod IP、DNS 及资源约束可观察；网络失败、资源申请失败和删除重试释放全部资源。 |
| [#6 对 kubelet 开放 vm](https://github.com/nautilus-runtime/nautilus/issues/6) | #4、#5 | 启用后通过 CRI 发现 handler；真实 kubelet 验证 RuntimeClass、容器与流式操作、重启恢复和资源清理；复测原有 handler。 |

首版范围为 Linux CPU 工作负载。GPU 直通、跨主机迁移和 VM 快照需要独立的资源与一致性设计。Firecracker 和 Dragonball 保留为后续 VMM 候选。工作负载兼容性由 CRI 验收结果决定，节点探针与性能测量单独记录。
