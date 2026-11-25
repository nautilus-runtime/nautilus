# Nautilus CRI validation results (critest)

- 被测服务为 `nautilusd`。
- 各轮使用的 kubernetes-sigs/cri-tools 版本和默认跳过数列于对应验证记录。
- 跑法:`critest --runtime-endpoint=unix:///run/nautilus/nautilus.sock --image-endpoint=unix:///run/nautilus/nautilus.sock`

## 2026-11-25 后台工作负载自动回收

在 pro6000 的独立 Debian 13 节点（Linux 6.12.118、Cloud Hypervisor v55.0）验证 `nautilus run -d --rm`。OCI、Registry WASI 和 VM 均返回容器 ID，并在执行结束后回收容器及其隐式 Pod。VM 执行期间可见的宿主日志目录随后删除。CLI 对旧 daemon 在创建资源前拒绝此选项。

强制终止 daemon 后，WASI 运行实例在重启期间收敛为已退出并完成回收。另一次在独立测试服务中保留 OCI 原进程，记录 daemon PID 变化，并确认相同的容器 init PID 在新 daemon 就绪后仍存活。原进程退出后，新 daemon 回收其隐式 Pod、容器、宿主日志目录和 OCI 运行状态。

首轮 `critest` v1.37.0 的安全上下文用例因公开 Registry CDN 拉取超时失败。预拉取该用例镜像后，对同一候选版本和 socket 重跑未过滤的 Runtime 与 Image 套件，105 通过、0 失败、37 跳过，耗时 264.137 秒，结束时 CRI Pod 和容器数均为零。Linux 串行 Rust 测试为 114 通过、0 失败、8 项忽略。严格 Clippy 保持基线的 29 项诊断，全库格式检查仍报告存量差异。

## 2026-11-25 独立 CLI 与 kubelet 共用 socket

在 pro6000 的独立 Debian 13 节点（Linux 6.12.118、Cloud Hypervisor v55.0）用同一 daemon 启动真实 kubelet v1.37.2 和 Nautilus CLI。标准 CRI 清单返回 Kubernetes 创建的原生 OCI 和 VM 容器，CLI 创建的 WASI 容器仅在 CLI 清单可见。经过 65 秒，独立 WASI 容器仍可读出此前已经退出的进程日志，两个 Kubernetes Pod 保持 Running。连续十次前台 `--rm` 与一次显式删除均成功。

包含 CLI 所有权和后台回收候选的联合构建运行完整 `critest` v1.37.0，同一 Unix socket 同时作为 Runtime 和 Image 端点。105 通过、0 失败、37 跳过，耗时 294.146 秒，结束时 CRI Pod 和容器数均为零。独立测试该所有权变更的 Linux Rust 串行套件为 113 通过、0 失败、8 项忽略。严格 Clippy 保持基线的 29 项诊断，全库格式检查仍报告存量差异。

## 2026-11-25 CRI 容器生命周期事件（GetContainerEvents）

在 pro6000 的独立 Debian 13 嵌套 KVM 节点（Linux 6.12.118、Cloud Hypervisor v55.0）验证 `GetContainerEvents`。双并发 CRI 订阅端分别验证了 OCI（退出码 37）、WASI（退出码 29）、Pod VM（退出码 35）、强制删除运行中 OCI 容器与 Pod VM 级联停删五种路径，均按 `CREATED -> STARTED -> STOPPED -> DELETED` 顺序送达且携带对应 Pod 状态与同 Pod 容器快照（删除事件排除已删容器），重复 Stop/Remove 不产生重复事件，新订阅重连不回放历史事件。另一次验证直接终止运行中 VM 的 VMM，两路订阅在发起停删前均收到 STOPPED，快照将不可取得退出码的容器标为 UNKNOWN。结束时 CRI Pod 与容器数归零。

真实 Kubernetes v1.37.2 kubelet 启用 `--feature-gates=EventedPLEG=true` 后，`/configz` 报告 `EventedPLEG=true`，`/metrics` 报告 `kubelet_evented_pleg_connection_success_count 1` 与 `kubelet_evented_pleg_connection_error_count 0`；OCI（exit 37）、Cloud Hypervisor v55.0 Pod VM（exit 35）、TLS Registry 打包 WASI（exit 0）以及运行中删除的 OCI Pod 均触发 `Evented PLEG: requested pod relist after container termination` 并匹配对应 Pod UID 与容器 ID。

完整未过滤 `critest` v1.37.0 对隔离 daemon 的同一 Unix socket 同时测试 Runtime 和 Image 端点，**105 通过、0 失败、37 跳过**，耗时 308.946 秒，结束时 CRI Pod 与容器数均为零。首次复验因测试节点上两个 bridge 使用同一子网，四项联网用例超时。改用独立子网后整套通过。Linux 离线串行 Rust 测试 112 通过、0 失败、8 项默认忽略，含 4 项事件状态机与滞后订阅单测。全库格式检查报告 31 个文件有差异，严格 Clippy 报告与基线相同的 29 项诊断。

## 2026-11-25 OCI 镜像中的 WASI 命令

Kubernetes v1.37.2 在 pro6000 的独立 Debian 13 节点（Linux 6.12.118）通过 `wasi` RuntimeClass 从 TLS Registry 拉取多架构镜像。Pod 成功退出，`kubectl logs` 返回模块输出。第二个镜像只有 OCI CMD，入口经镜像内绝对符号链接解析，`kubectl logs` 与 `nautilus logs` 返回相同内容。单独运行的 CLI 也验证了镜像默认命令参数和环境变量以及请求覆盖。

日志轮转后、CRI reopen 前写入的新文件内容被保留。重启 daemon 会清理上次中断的暂存目录和无容器记录的发布目录，并保留已有容器的文件。删除 Pod 后，CRI Pod、容器和暂存目录均归零。完整 `critest` v1.37.0 联合回归见下一节。Linux 单线程 Rust 测试为 108 通过、0 失败、8 项默认忽略。全库格式检查仍有存量差异，严格 Clippy 报告与基线相同的 29 项诊断。

## 2026-11-25 OCI 镜像元数据校验

在 pro6000 的独立 Debian 13 节点（Linux 6.12.118）以 OCI WASI 联合构建验证，共用的 CRI ImageService 从 TLS Registry 拉取多架构镜像。分别篡改 registry 返回的 manifest digest、index 所选子 manifest 和 config JSON，三种拉取均被拒绝。使用 SHA-512 校验 config descriptor 的镜像仍以配置原始字节的 SHA-256 作为 CRI ImageID，kubelet v1.37.2 用该镜像成功启动 WASI 命令。

联合构建在同一 Unix socket 同时测试 Runtime 和 Image 端点。完整 `critest` v1.37.0 为 105 通过、0 失败、37 跳过，耗时 294.798 秒，结束时 Pod 与容器列表均为空。

## 2026-11-25 WASI 异步生命周期

在 pro6000 的独立 Debian 13 嵌套 KVM 节点（Linux 6.12.118）验证。忙循环和等待 WASI 时钟的模块均在仍然运行时返回容器 ID，随后从 CRI 停止并得到退出码 137。重复停止成功。前台输出和 Ctrl-C 后 `--rm` 回收通过。强制终止 daemon 后，重启在同一状态库中将原实例标为 `RuntimeRestarted`，最后 CRI Pod 和容器列表均为空。

完整 `critest` v1.37.0 使用同一 Unix socket 的 Runtime 和 Image 端点，105 通过、0 失败、37 跳过，耗时 687.680 秒。随后将入口点检查保留在 Start 调用以恢复 Create 的旧语义。该修订通过 Linux 单元测试。

## 2026-11-25 OCI、WASI 和 VM CLI

在 pro6000 的独立 Debian 13 嵌套 KVM 节点（Linux 6.12.118、Cloud Hypervisor v55.0）使用真实 daemon 验证。OCI 与 VM 的前台命令分别返回独立 stdout、stderr 与退出码 37。WASI 本地模块收到参数并返回输出与退出码 29。无输出的 OCI 命令、`-d` 后执行 `exec`、前台收到 Ctrl-C 后退出 130，以及 `--rm` 回收隐式 Pod 均通过。

完整 `critest` v1.37.0 使用同一个 Unix socket 作为 Runtime 和 Image 端点，105 通过、0 失败、37 跳过，耗时 292.075 秒。套件结束时 CRI Pod 与容器数均为零。

## 2026-11-25 CRI 有限列表流

在 pro6000 的独立 Debian 13 嵌套 KVM 节点（Linux 6.12.118）运行未过滤的 `critest` v1.37.0。Runtime 和 Image 端点使用同一个 daemon socket。包含五个列表流的初版代码通过 105 项，0 失败，37 跳过，耗时 419.113 秒。

随后用真实 gRPC 客户端对同一 daemon 上的 OCI 容器、Cloud Hypervisor v55.0 VM 容器及已拉取镜像调用五种列表流，核对 ID、父 Pod、AND 标签筛选、空结果、无重复和 EOF。该用例还发现普通容器列表漏用标签，修复后同时复验普通查询和列表流。

## 2026-11-25 Pod VM 的 CRI 用量统计

在两个隔离节点用提交 `0b1e188` 的同一 daemon 二进制运行未过滤的 critest，检验默认 OCI 后端。每轮的 runtime 和 image endpoint 均指向该节点专用的 Unix socket。两轮退出码均为 0，清理后各自的 CRI Pod 和容器数均为零。

| critest 版本 | 通过 | 失败 | 跳过 | 耗时 |
|---|---:|---:|---:|---:|
| v1.31.1 | 96 | 0 | 17 | 237.372 秒 |
| v1.37.0 | 105 | 0 | 37 | 321.110 秒 |

提交 `0b1e188` 在独立的 Debian 13 嵌套 KVM 节点通过双 Pod 验收，耗时 13.50 秒。节点使用 Linux 6.12.118、Cloud Hypervisor v55.0 和 virtiofsd 1.13.2，guest 使用 Linux 5.15.0-191。通过 CRI 在同一 Pod 的一个容器内写入 16 MiB tmpfs，再由 exec 子进程对该文件执行 12 次 SHA-256。该容器的累计 CPU 为 677,363,000 ns，内存为 19,103,744 字节。同 Pod 空闲容器分别为 24,089,000 ns 和 2,019,328 字节。

四个 CRI 用量接口均经过真实 daemon 验证。Pod 统计包含两个容器和 Pod 父 cgroup 的用量。列表按 ID、Pod 和标签过滤，另一台空 Pod 可独立取样。切断首个 Pod 的 guest 连接后，该容器的单项查询报错，另一台 Pod 仍能返回用量。[CRI 验收用例](../tests/vm_cri_workloads_test.rs)

独立节点的 kubelet v1.37.2 在 `/configz` 报告 `PodAndContainerStatsFromCRI=true`。通过 Kubernetes API 的 Node 代理读取 `/stats/summary`，其中 alpha Pod、web 和 console 容器的工作集分别为 17,350,656、17,125,376 和 217,088 字节。beta Pod 和 client 容器分别为 290,816 和 286,720 字节。重启 daemon 后两个 Pod 保持原有身份。强制终止 beta 的 VMM 后，alpha 仍能处理请求。删除 Pod 后，VMM、virtiofsd、网络命名空间和 IPAM 租约均已回收，CRI Pod 列表为空。[kubelet 验收用例](../tests/kubelet_vm_acceptance.py)

Linux 完整 Rust 测试为 104 通过、0 失败、8 项默认忽略。与主线 `12e7d5c` 对照，全库 `cargo fmt --check` 仍报告相同的 31 个文件，`cargo clippy --locked -- -D warnings` 仍报告相同的 29 项诊断。

复查 KVM 夹具时发现，最后一次重启 daemon 后，CRI 列表已清空，但三个独立运行各留下一对属于内部就绪探针的 VMM 和 virtiofsd 服务。原夹具在探针运行时终止 daemon，并开始删除临时状态目录。加入宿主资源断言后，同一场景在 13.76 秒后如实报错，保留了完整目录。分别在探针运行时发送 SIGKILL 和 SIGTERM，再从原状态目录重启，daemon 均回收了旧服务、网络命名空间、IPAM 租约和归属记录。

修正后的夹具在 CRI 公告 `vm` handler 后继续执行，关闭 daemon 后复查该次测试的资源。真实 KVM 连续三轮通过，耗时 17.84、17.33、17.22 秒。第四轮由另一 daemon 同时运行独立的 VM Pod，用例在 17.11 秒内通过。运行前后的 systemd 服务、网络命名空间、IPAM 租约和归属记录一致，独立 Pod 保持 Ready，VMM 进程 ID 未变。[夹具修复记录](https://github.com/nautilus-runtime/nautilus/issues/34)

## 2026-11-24 真实 kubelet 和 RuntimeClass

在 pro6000 的独立 Debian 13 嵌套 KVM 节点验证提交 `58e5ea5`。API server、scheduler、controller-manager、kubelet 与 kubectl 均为 Kubernetes v1.37.2，节点为 Linux 6.12.118、systemd 257.13、Cloud Hypervisor v55.0 和 virtiofsd 1.13.2，guest 为 Linux 5.15.0-191。测试通过 Kubernetes API 提交两个 VM Pod，在三个 Ready 容器上核对日志、镜像 ID、同 Pod 共享和跨 Pod 独立的 boot ID、跨 Pod HTTP、原 init 的交互式 attach、端口转发和 `kubectl exec` 传回的退出码 35。[复验脚本](../tests/kubelet_vm_acceptance.py) [RuntimeClass 和节点配置](VM_RUNTIMECLASS.md)

两个 Pod 的地址为 `10.232.0.41` 与 `10.232.0.40`。强制终止再启动 daemon 后，两个 IP、两个 boot ID 和三个容器 ID 均保持。随后向第二个 VMM 发 SIGKILL，第一个 Pod 保持原 boot ID，仍能通过 kubectl 路径提供 HTTP。kubelet 删除两者后，测试命名空间的 CRI sandbox 数归零，VMM、virtiofsd、owner 日志、网络命名空间与 IPAM 租约回到测试前状态。

guest 与宿主均读到对方写入的 hostPath 文件。ConfigMap 在 guest 中只读，kubelet 切换投影后，轮询重新打开文件取得新值。宿主 cgroup 报告 1 个 CPU、512 MiB 上限和 `memory.oom.group=1`，guest 检出一个 vCPU 和扣除开销后的内存。CRI 容器与 Pod 的批量用量查询均省略 guest 条目。默认 OCI Pod 保持 Ready，进程位于 kubelet 指定的 systemd scope。独立 WASI 模块通过同一 daemon 运行，CRI 记录退出码 23。

在同一 daemon 替换测试 CNI 配置并触发一次 VM 创建失败，CRI 和 Node 均撤回 `vm`，修复配置后两者自动恢复。以升级前的运行中 VM 验证，daemon 升级后保持 guest boot，依据 MainPID 持有的 OFD 锁登记原 inode 131691，随后 VMM 被强制终止，整个 attempt 可回收。另一台升级前已退出的 VM 保留了遗留锁的原 inode 131687，Pod 和原 IP 均成功回收。[VMM 锁的恢复规则](VM_RUNTIME.md#首版使用系统级-systemd-托管-vmm)

新版 critest v1.37.0 首轮为 101 通过、4 失败、37 跳过。两个失败涉及镜像配置 ID，另两项涉及并发 RemoveImage 和 sandbox attempt。修复后、加入 VMM 锁恢复前的 `385b34d` 为 105 通过、0 失败、37 跳过，v1.31.1 为 96 通过、0 失败、17 跳过。最终在 `58e5ea5` 上分别以同一 daemon 的 Unix socket 作为 runtime 和 image endpoint，未过滤的 **v1.37.0 为 105 通过、0 失败、37 跳过**，耗时 320.568 秒，**v1.31.1 为 96 通过、0 失败、17 跳过**，耗时 235.510 秒。另用 v1.31.1 聚焦 HostNetwork=false 的快速退出清理用例，1 通过、0 失败、112 项未选中，耗时 5.032 秒。两个独立节点在各自套件结束后均通过 CRI 确认 Pod 和容器数为零。该提交运行完整 Rust 测试得到 104 通过、0 失败、8 项默认忽略。全库 fmt 涉及与主线基线相同的 31 个文件，Clippy 在 `-D warnings` 下均报告 29 项诊断。

## 2026-11-24 Pod VM 宿主卷

提交 `66c4bc0` 在 pro6000 的 Linux 6.12.118 嵌套 KVM 节点上通过完整 critest v1.31.1。runtime 和 image endpoint 共用隔离 daemon 的 Unix socket，**96 通过、0 失败、17 跳过**，耗时 229.026 秒。首轮测试中节点的镜像仓库代理失去监听，84 项在镜像拉取时失败，12 项通过、17 项跳过。恢复代理后用同一 daemon 二进制、新状态目录和相同随机种子重跑，结束时通过 CRI 确认 Pod 和容器计数均为零。

同一提交的 Linux Rust 测试为 96 通过、0 失败、8 项默认忽略。通过独立 CRI 验收，在 Cloud Hypervisor v54.1 与 virtiofsd 1.13.2 上验证了同一普通文件以不同读写权限挂入不同容器、宿主与 guest 的双向修改、kubelet 投影目录的原子切换、两个 Pod 的隔离以及 daemon 和文件系统服务异常退出后的回收。持久化占位身份后、发布前的中断用例也验证了同名无关文件的 inode 得以保留。该 KVM 用例耗时 12.81 秒。[挂载模型和限制](VM_RUNTIME.md)

## 2026-11-24 Pod VM 工作负载和网络

在 pro6000 的 Linux 6.12.118 嵌套 KVM 节点，runtime 和 image endpoint 指向同一个隔离 daemon。对 PR #24 运行未过滤的 critest v1.31.1。提交 `302d543` 得到 96 通过、0 失败、17 跳过，耗时 244.594 秒。提交 `714ae01` 得到 **95 通过、1 失败、17 跳过**，耗时 236.036 秒。失败发生在默认 OCI 后端，HostNetwork=false 的 AfterEach 对已快速退出的容器执行 Stop 时，systemd 已回收其 youki scope。较早提交的完整套件与聚焦运行均复现过。后续提交仅调整静态错误值的写法和格式。[修复跟踪](https://github.com/nautilus-runtime/nautilus/issues/25)

`b7eb13f` 的 Linux Rust 测试为 96 通过、0 失败、8 项默认忽略。全库格式检查仍涉及基线的 31 个文件，Clippy 仍报告 29 条诊断。独立运行的真实 CRI 测试使用 Cloud Hypervisor v55.0，覆盖双 Pod 网络与 DNS、两个 OCI 容器、十次 ExecSync、超时和取消、guest 连接中断、daemon 被强制终止后的恢复、日志及重复清理。Cloud Hypervisor v54.1 在 Linux 5.15.0-191 物理宿主上另行通过双 Pod 网络测试。[运行步骤](VM_RUNTIME.md)

## 2026-11-24 VM 的交互式 CRI

在 pro6000 的隔离节点使用 crictl v1.37.0、Cloud Hypervisor v55.0 和实际 guest，通过 CRI 单独核对 Exec、Attach、PortForward。修订前标准输入的 3 MiB 二进制内容经 guest cat 只返回 2,387,968 字节。修订后 3,145,728 字节逐字节一致。另验证了独立 stderr、非零退出码、终端尺寸、重连到原 init、客户端断开时取消执行、目标端口不存在、连续转发两个连接和 guest 失联的错误。在单独的 SPDY 测试中，一条流半关闭后仍能读取，将其复位后另一条流仍能传输。

`347c4dc` 上默认 OCI 的未过滤 critest v1.31.1 为 **95 通过、1 失败、17 跳过**，耗时 245.738 秒。唯一失败是 HostNetwork=false 清理快速退出容器时缺失 systemd scope。合入 #25 对应修复后，在最终组合 `a594097` 的新状态目录重跑，结果为 **96 通过、0 失败、17 跳过**，耗时 234.533 秒。单独运行随项目引入的 SPDY 测试，36 项通过。Cloud Hypervisor v55.0 的双 Pod 测试在 15.94 秒内完成卷、流、daemon 恢复和清理的联合验收。[协议及验证步骤](GUEST_OCI_PROTOCOL.md)

## 2026-11-24 普通 OCI 容器快速退出

在独立的 Debian 13 节点（Linux 6.12.118、systemd 257、cgroup v2）上，提交 `b7eb13f` 的 `HostNetwork is false` 用例第二轮复现清理失败。daemon 已记录 init 的退出码 0，随后 systemd 回收了其 scope，StopContainer 枚举 cgroup 进程时收到 ENOENT。

提交 `ae6e5b1` 针对同一用例连续运行 8 轮，每轮均为 1 通过、0 失败、112 跳过。同一隔离 daemon 的 Unix socket 用作 runtime 和 image endpoint，完整 critest v1.31.1 为 **96 通过、0 失败、17 跳过**，耗时 237.543 秒。通过 CRI 复查，测试结束后 Pod 与容器数量均为零。另用 12 个退出码为 37 的原生 OCI 容器执行 24 次 StopContainer 和 24 次 RemoveContainer，重复停止后退出码与结束时间均保持一致。[问题记录](https://github.com/nautilus-runtime/nautilus/issues/25)

此前一次验证遇到镜像仓库代理中断，得到 12 通过、84 失败、17 跳过。代理恢复后使用新的状态目录完成验证。Linux Rust 测试 97 通过、8 项默认忽略。与基线 `20d68ed` 对照，fmt 报告同样的 31 个文件，clippy 在 `-D warnings` 下报告同样的 10 项诊断。

## 2026-11-24 Pod VM 生命周期

PR #10 的生产代码提交为 `af327ce`。用 Cloud Hypervisor v54.1 在 Linux 5.15.0-191 物理宿主验证，再用 v55.0 在同机的 Linux 6.12.118、systemd 257 嵌套 KVM 节点复验。每轮启动两个实验性 Pod VM，再通过真实 daemon 的 CRI socket 验证 guest boot ID、强制终止并重启 daemon、并发及重复 Stop/Remove、意外退出后的清理。[实验范围与运行命令](VM_RUNTIME.md#首版使用系统级-systemd-托管-vmm)

在同一嵌套节点上将 runtime 和 image endpoint 都指向隔离 daemon 的 Unix socket，运行未过滤的 critest v1.31.1。首轮冷缓存为 **95 通过、1 失败、17 跳过**，失败发生在 port-forward 用例从 GCR 拉取 nginx 镜像层时，尚未创建容器。恢复测试代理、预置 31 个按 SHA-256 核验的官方压缩层后，用全新的状态目录重跑整套，结果为 **96 通过、0 失败、17 跳过**，耗时 239.851 秒，退出码 0。17 项跳过包括 user namespace 6 项、递归只读挂载 4 项、SupplementalGroupsPolicy 2 项及默认关闭的 benchmark 5 项。该套件检验默认运行时，VM 行为另由上述双 Pod 验收覆盖。

在独立 Linux 网络命名空间以单测试线程执行 `cargo test --locked --offline`，84 项通过，3 项默认忽略。真实 VM 测试另行执行通过。额外执行两项默认忽略的 OverlayFS 测试，其中重新挂载视图的测试在本次提交和基线 `32d26c0` 上均读回了底层文件内容。两版的全库格式检查均涉及相同的 31 个文件，Clippy 均报告相同的 29 条诊断。

## 2026-11-24 拉取阶段解包

版本 `4666bb5` 通过 28 项库测试，包括解包失败后的重试、拉取日志恢复、快照复用、退役和延迟卸载。critest v1.31.1 运行镜像管理与 RemoveImage 幂等性相关的 9 项用例，9 项通过，其余 104 项未选中。独立 CRI 检查还验证了拉取超时后的四路重试和关闭 OverlayFS 后的完整生命周期。见[启动性能和测试边界](PERFORMANCE.md)。

## 2026-11-24 引擎升级验证

youki 0.8.0 通过 27 项库测试、13 项 OCI 安全配置测试和 2 项集成测试。另以 CRI 验证了 exec 退出码、工作负载退出码、NoNewPrivs、seccomp 过滤器继承、双向挂载传播及旧版容器升级。2026-08-28 的 96 项 critest 结果对应当时的引擎版本。本轮测量口径和原始数据见[性能报告](PERFORMANCE.md)。

## 基线演进

| 日期 | 结果 | 说明 |
|---|---|---|
| 2026-08-25 早 | 5 / 91 | 首次接入;根因 CRI Image.size=0 + id 填引用 |
| 2026-08-25 | 29 / 67 | size(manifest 层字节)+ identity 修复 |
| 2026-08-25 | 57 / 39 | 幂等 stop/remove、Image.uid、同 digest 归并、安全规格模块、apparmor 全绿 |
| 2026-08-25 | 65 / 31 | AppArmor 9/9 聚焦、seccomp SecurityProfile 字段提取 |
| 2026-08-26 | 72 / 24 | Image Manager 收口(引用定位、repo digests、pull deadline)|
| 2026-08-27 | **79 / 17** | DNS(search/options resolv.conf)、cap 名规范化+五 set 全加、privileged sandbox→容器继承全 caps、mount propagation 2/3(rslave/rprivate)|
| 2026-08-27 | **83 / 13** | fork-engine:seccomp 继承、cap ALL、ReadOnlyRootfs、hostPort |
| 2026-08-27 | **89 / 7** | **Streaming 5 用例全绿**(SPDY server:exec tty×2/attach/portforward×2)+ hostNetwork pod 语义修复(OOM 用例随之恢复)|
| 2026-08-28 | **95 / 1** | pod pidns 持有者 + pid/ipc namespace 语义、rbind/rshared 卷挂载、直 exec 无 wrapper、OOM 归因、EINTR 修复;余 1 为 attach 竞态(误标 AppArmor)|
| 2026-08-28 | **96 / 0 全绿(两连跑)** | **attach 终局修复**:honor CRI stdin(open_stdin 保持 init 存活,attach spawn 不再撞 Stopped)|

## 终局:96 / 0(2026-08-28,113 specs / 17 skipped 为 critest 自身架构跳过)

最后 1 个不稳定失败的全链路(每轮 ~1 个,漂移在不同 spec 名下):

- **表象**:critest 汇总把失败记在 "AppArmor permissive" 等无辜 spec 名下
  (`Expected "" to equal "hello"`)。
- **真相**:失败断言在 `streaming.go:290` —— **attach 用例**。其检测 goroutine
  (`Eventually(60s)`) 通过 `GinkgoRecover()` 把失败记到当时正在运行的下一个 spec。
- **机制**:attach 容器是 `Stdin:true` 的 `/bin/sh`。nautilus 原先不接 stdin →
  sh 瞬间 EOF 退出 → 容器 Stopped。attach 拨号时 libcontainer tenant exec 拒绝
  `incompatible container status: Stopped` → spawn 失败 → error 流通知路径又未
  送达(客户端把**空** error 流 EOF 解码为成功,见 client-go errorstream.go:
  `watchErrorStream` 空 payload → nil)→ `StreamWithContext` 干净返回 → attach
  spec "通过" → pod 被删 → goroutine 60s 必然超时 → 污染下一个 spec。
- **修复**:honor CRI `ContainerConfig.stdin` —— `open_stdin` 下 daemon 创建
  stdin 管道并把写半端停靠(`close_container_stdin` 于 stop/remove 释放),
  init 阻塞在空闲 stdin 上保持 Running(与 containerd 行为一致)→ attach spawn
  稳定成功。
- 佐证:containerd 能过该用例正因其为 stdin 容器保持 stdin 基础设施,init 不会提前退出。

## 2026-08-28 89→96 全过程要点

| 类别 | 修复 |
|---|---|
| NamespaceOption HostPid/HostIpc + pid=POD | pod 级 PID ns 持有者(`fork_pidns_holder`,parked grandchild + bind-mount ns 句柄);容器 `__POD_PIDNS__` 占位替换;pid=NODE/CONTAINER 独立 ns;host ipc 依赖 `kernel.shm_rmid_forced=0`(已持久化 sysctl.d) |
| Mount readonly/rshared | 卷挂载改 `rbind`;rshared 需要 fork 侧 prepare_rootfs 放行 → fork 分支 `fix/rootfs-propagation-recursive` (2b4cdda) |
| fork 回归排查 | b51cb5c/6a0d9e8(纯 SIGCHLD init-clone)导致 tenant waitpid/reaper ECHILD,git bisect 定位后弃用,回 2867015 + rshared 补丁 |
| 直 exec | 去掉 shell wrapper:workload 直 exec 为容器 init/tenant,退出码诚实、pid1 语义正确 |
| stdio 管道 | socketpair 不可经 /proc/self/fd 重开(ENXIO)→ 全改 pipe;tokio UnixStream 包 pipe 报 ENOTSOCK;AsyncFd(epoll)+fork 会卡死 gRPC accept loop → **阻塞线程 + EINTR 重试**(read/write 均) |
| OOM 归因 | start 时 system.slice oom_kill 基线 + 30ms scope memory.events 轮询 + `oom_killed()` 兜底;exit 137 → reason=OOMKilled |
| fd 泄漏 | pidns 持有者继承全部 daemon fd → 全量跑挂 EMFILE;`close_out_inherited_fds()` 关闭 >2 |
| 网络 | 宿主残留 docker br-\* 桥占 172.20.0.0/16 与 nautilus0 冲突 → 删除后 multi-container 网络恢复 |
| 可观测 | 流服务器会话/流/tenant 生命周期插桩;exec 泵 EINTR 重试与退出原因日志;`tracing-log` 桥让 spdystream-rs 内部日志可见 |

## Streaming 实现纪要(2026-08-27,5/5)
- `src/cri/streaming.rs` + `spdystream-rs 0.1.2`;SPDY remotecommand v4 +
  portforward.k8s.io;三个 CRI RPC 返回 `http://127.0.0.1:{port}/{mode}/{token}`
- 协议要点:token 承载全部会话参数(critest 不回传查询参数);通道由
  `streamType` 头区分;101 必须回显 X-Stream-Protocol-Version;exit code 走
  error 流 metav1.Status;v1 portforward 为纯字节中继;请求流不带 `:path`
  伪头 → 用 parse_upgrade_request/Connection::serve 自行拼装以解析路径
- 后端 `SandboxBackend::exec_stream`:libcontainer tenant + socketpair stdio,
  WNOHANG reap;断连 SIGKILL 防泄漏
- 已知限制:attach=容器内新起 /bin/sh(主进程 stdio 文件化,无法注入 stdin);
  tty 无真实 PTY(critest 断言仅内容);debug 日志级别下 libcontainer 行会泄入
  exec 输出(INFO 干净)
- (attach 客户端 goroutine 偶发竞态已由 stdin honor 修复终结,见上 96/0 终局)

## 关键经验 / 已修复的坑
- libcontainer 0.7 seccomp 加载路径:spec 省略 noNewPrivileges → 前置加载(cap-drop 前)成功;
  有 NNP 键 → 后置加载 → EPERM
- tenant exec 的 seccomp/readonlyPaths 等 spec 应用:已通过 fork libcontainer 解决
  (Bevisy/youki rev 25338b60 起 tenant 保留 seccomp/maskedPaths/readonlyPaths/rootfsPropagation)
- runc-CLI exec 读不了 libcontainer 0.7 的 state(工厂 panic) — 不能用作 exec 退路
- cap 名须 CAP_ 前缀;adds 须进全部五个 capability set,否则 drop_privileges 报错
- privileged 沙箱必须把 privileged 透传给无 security_context 的容器

## 2026-08-27 fork-engine 方案(82/14)

- **fork libcontainer**:Bevisy/youki rev 25338b60 — tenant(exec) 保留
  seccomp/maskedPaths/readonlyPaths/rootfsPropagation;nautilus [patch] 指向 fork
- 成果:seccomp default+localhost ✓(chmod 400 / 被拦)、cap ALL add/drop ✓
  (CAP_ALL normalize 展开 41 全量)、ReadOnlyRootfs ✓(dual-stream stdout/stderr 分离)、
  exec inherits apparmor/NoNewPrivileges
- 附属:OOMKilled reason 已实现(exit137+cgroup oom_kill),用例仍挂因 busybox dd
  不真正超限(环境/命令语义,非实现缺陷);AppArmor enforce 3 项仍挂(exec 的 apparmor
  profile 应用在 tenant 中需进一步验证)
- hostPort 通过(loopback forwarder 抽公共 helper,CRI 沙箱路径同享)→ **83/13**
- 剩余 13:Streaming 5 · NamespaceOption 2(exec ns 死锁) · AppArmor enforce 3 ·
  OOM 1(busybox dd 不超限) · Mount readonly 1(mount 语义) · rshared 1(引擎限制)

## 剩余 13 项逐条根因(2026-08-27 深挖结论)
- Streaming 5(exec tty ×2 / attach / portforward ×2):CRI v1.31 用 SPDY remotecommand,
  需实现 streaming server(网络层自研,数千行)→ 下轮专项
- ContainerPID / HostIpc 2:独立 pid/ipc ns 容器 exec 在 libcontainer 消息握手 hang
  (非 seccomp——nnp true-only 修正后仍挂)→ fork 深修
- AppArmor enforce 3:profile 已 load、bundle apparmorProfile 正确、with_apparmor 已传,
  但 exec 进程未进容器 AppArmor 域(宿主 LSM×tenant 应用点)→ 待查 apply 静默失败
- OOMKilled 1:reaper+cgroup 事件检测已实现;critest 命令 dd bs=20M 在
  busybox+8/16MB cgroup 下秒退 exit0(未真正 OOM,cgroup MemoryMax 已由 systemd 设置)→ 环境/命令语义
- Mount readonly 1:ro bind 生效(touch 被拒),但用例需容器内 tmpfs 子挂载可写,
  exec mount 语义待核
- rshared 1:libcontainer prepare_rootfs 拒绝 rshared rootfsPropagation(engine)
