# Nautilus 开发路线图

## 当前能力

截至 2026-11-26，kubelet 和 Nautilus CLI 通过同一 CRI v1 socket 管理三类工作负载。节点默认可运行原生 OCI 容器和 WASI Preview 1 命令。按 [Pod VM 节点指南](VM_RUNTIMECLASS.md) 配置 KVM、系统级 systemd、Cloud Hypervisor、virtiofsd、guest 镜像和网络后，kubelet 可以通过 RuntimeClass 为每个 Pod 创建独立的 Linux guest。

| 范围 | 已验证的行为 |
|---|---|
| 运行与交互 | OCI 和 Pod VM 支持连接容器 init 的交互输入、Attach 和 PTY。Pod VM guest 连接恢复后，首次新建交互 exec 或 Attach 即可使用，无需先查询容器状态，且支持二进制标准输入。WASI 命令支持前台标准输入，OCI 打包命令提供 CRI 日志。CLI 支持前后台运行与自动回收。[工作负载示例](../README.md#what-you-can-run) |
| Kubernetes | 在 Kubernetes v1.37.2 上实测了 VM RuntimeClass、Pod 网络、日志、exec 和 daemon 重启恢复。原生 OCI 的 Guaranteed 和 Burstable 容器可以原地调整 CPU 和内存。[节点验收](VM_RUNTIMECLASS.md) · [CRI 验证记录](CRITEST_RESULTS.md) |
| 镜像和存储 | PullImage 缓存压缩层并准备可复用的镜像树。流式解包覆盖 tar、gzip 和 zstd。21 层镜像的配对测试中，两版各完成 110 个 VM Pod、1,100 次热 ExecSync，清理并发为 4；清场后 daemon PSS 从 705.0 MiB 降到 71.9 MiB。[测量条件](VM_AGENT_LOOP.md#daemon-解包内存复验) |
| 网络与安全 | 原生 OCI 支持 CNI、hostPort、seccomp 和 AppArmor。Pod VM 当前使用 IPv4 bridge 和 host-local CNI。Wasm 策略可在创建 Pod、创建容器和启动容器时检查请求。[架构](ARCHITECTURE.md) · [VM 设计](VM_RUNTIME.md) |

完整未过滤的 `critest` v1.37.0 在同一隔离 Unix socket 上验证 RuntimeService 与 ImageService，结果为 **107 通过、0 失败、35 跳过**。同轮 Linux 串行 Rust 测试 136 通过、0 失败、8 项忽略。Pod VM 流式恢复用上游 `crictl` 另行验证。两套测试的覆盖范围和历史 v1.31.1 基线均见[验证记录](CRITEST_RESULTS.md)。

## 待完成工作

[CRI 功能跟踪 #36](https://github.com/nautilus-runtime/nautilus/issues/36) 及其子任务按后端定义验收条件。

| 工作 | 剩余范围 |
|---|---|
| [原生 OCI hostNetwork #73](https://github.com/nautilus-runtime/nautilus/issues/73) | 补齐 CRI 状态中的宿主网络命名空间，验证真实 kubelet 持续运行 Pod 及 daemon 重启后的状态。 |
| [CRI 指标 #40](https://github.com/nautilus-runtime/nautilus/issues/40) | VM guest 已上报 CPU 时间、节流、内存分项与页错误。后续补齐文件系统归属、块 I/O、Pod 网络与进程指标，并经 kubelet 核对。 |
| [资源原地调整 #41](https://github.com/nautilus-runtime/nautilus/issues/41) | 已支持原生 OCI 容器的 CPU 和内存调整。接下来实现 Pod 级通知与 guest 限制更新，分别验证 guest、VMM 和宿主的容量变化。 |
| [Checkpoint 与 Restore #42](https://github.com/nautilus-runtime/nautilus/issues/42) | 实现原生容器和 Pod 的一致性捕获、恢复和失败回收，再验证 VM 存储及设备的一致性。 |

2026-11-24 的 BusyBox OCI 对照中，预热串行创建加启动 P95 为 47.3 ms，四路并发为 92.2 ms。Pod VM 在单节点连续完成 6 波，每波 16 路并发启动与回收，共 96 个 Pod、960 次热 ExecSync，整波回收 P95 为 812 ms。长时间运行和容量上限仍待测量。[原生 OCI 测量](PERFORMANCE.md) · [Pod VM 测量](VM_AGENT_LOOP.md)

2026 年 8 月还记录了 e2e 等待 `wait4` 偶发挂起、libcontainer 的 DEBUG 日志进入 exec 输出。后续复查应保留负载、日志级别和阻塞点，便于与近期的 VM streaming 及并发回收问题区分。

中长期候选方向包括 Wasm 策略的 WIT/component 接口、镜像签名与验签、镜像 GC、状态存储评估、机密计算和兼容性认证。GPU 透传单独跟踪，已有 CPU/KVM 数据的覆盖范围见 [VM 设计](VM_RUNTIME.md)。

## 验证要求

按 [AGENTS.md](../AGENTS.md) 在 Linux 运行 `cargo fmt --check`、`cargo clippy --locked -- -D warnings` 和 `cargo test --locked`。2026-11-25 的严格 Clippy 检查仍有 29 项基线诊断，格式检查涉及 31 个基线文件。

本轮稳定性验证覆盖 [VM 流式交互恢复](https://github.com/nautilus-runtime/nautilus/issues/65)、[16 路 VM Pod 并发回收](https://github.com/nautilus-runtime/nautilus/issues/66) 与 [CRI 日志轮转时保留新日志](https://github.com/nautilus-runtime/nautilus/issues/68)。各轮环境、结果及限制见[验证记录](CRITEST_RESULTS.md)。

CRI 可见变更还需对实际 daemon 的 Runtime 和 Image socket 运行完整 critest，分别记录通过、失败与跳过数。VM 生命周期变更需记录宿主内核和 VMM 版本，并经 CRI 验证路由、状态及清理。涉及 Kubernetes 可见行为时用真实 kubelet 复测。
