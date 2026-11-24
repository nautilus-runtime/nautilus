# Nautilus CRI validation results (critest)

- 被测服务为 `nautilusd`。
- 测试工具为 kubernetes-sigs/cri-tools 的 `critest v1.31.1`。
- 跑法:`critest --runtime-endpoint=unix:///run/nautilus/nautilus.sock --image-endpoint=unix:///run/nautilus/nautilus.sock`

## 2026-11-24 Pod VM 宿主卷

提交 `66c4bc0` 在 pro6000 的 Linux 6.12.118 嵌套 KVM 节点上通过完整 critest v1.31.1。runtime 和 image endpoint 共用隔离 daemon 的 Unix socket，**96 通过、0 失败、17 跳过**，耗时 229.026 秒。首轮测试中节点的镜像仓库代理失去监听，84 项在镜像拉取时失败，12 项通过、17 项跳过。恢复代理后用同一 daemon 二进制、新状态目录和相同随机种子重跑，结束时通过 CRI 确认 Pod 和容器计数均为零。

同一提交的 Linux Rust 测试为 96 通过、0 失败、8 项默认忽略。通过独立 CRI 验收，在 Cloud Hypervisor v54.1 与 virtiofsd 1.13.2 上验证了同一普通文件以不同读写权限挂入不同容器、宿主与 guest 的双向修改、kubelet 投影目录的原子切换、两个 Pod 的隔离以及 daemon 和文件系统服务异常退出后的回收。持久化占位身份后、发布前的中断用例也验证了同名无关文件的 inode 得以保留。该 KVM 用例耗时 12.81 秒。[挂载模型和限制](VM_RUNTIME.md)

## 2026-11-24 Pod VM 工作负载和网络

在 pro6000 的 Linux 6.12.118 嵌套 KVM 节点，runtime 和 image endpoint 指向同一个隔离 daemon。对 PR #24 运行未过滤的 critest v1.31.1。提交 `302d543` 得到 96 通过、0 失败、17 跳过，耗时 244.594 秒。提交 `714ae01` 得到 **95 通过、1 失败、17 跳过**，耗时 236.036 秒。失败发生在默认 OCI 后端，HostNetwork=false 的 AfterEach 对已快速退出的容器执行 Stop 时，systemd 已回收其 youki scope。较早提交的完整套件与聚焦运行均复现过。后续提交仅调整静态错误值的写法和格式。[修复跟踪](https://github.com/nautilus-runtime/nautilus/issues/25)

`b7eb13f` 的 Linux Rust 测试为 96 通过、0 失败、8 项默认忽略。全库格式检查仍涉及基线的 31 个文件，Clippy 仍报告 29 条诊断。独立运行的真实 CRI 测试使用 Cloud Hypervisor v55.0，覆盖双 Pod 网络与 DNS、两个 OCI 容器、十次 ExecSync、超时和取消、guest 连接中断、daemon 被强制终止后的恢复、日志及重复清理。Cloud Hypervisor v54.1 在 Linux 5.15.0-191 物理宿主上另行通过双 Pod 网络测试。[运行步骤](VM_RUNTIME.md)

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
