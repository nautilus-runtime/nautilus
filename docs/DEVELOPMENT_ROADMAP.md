# Nautilus 开发路线图

> MVP 状态记录于 2026-08-29，性能基线更新于 2026-11-24。**MVP 阶段已完成**(原 2025-10 的 8-10 周阶段计划与
> 任务分解文档已随阶段结束清理)。现状以本文档与
> [架构文档](ARCHITECTURE.md)、
> [critest 测试结果](CRITEST_RESULTS.md)为准。

## 📊 当前状态(2026-08,MVP 阶段完成)

| 维度 | 现状 | 原目标 |
|---|---|---|
| CRI 一致性 | **critest v1.31.1 96/0 全绿**(两连跑确认) | crictl 命令可用 |
| 单元/集成测试 | cargo test 6 套件 28/28 绿 + e2e 5(e2e 偶发 wait4 挂起待查) | 套件绿色 |
| 空闲内存 | **PSS 10 MiB**(RSS 11 MiB / 12 线程,release 实测) | < 100MB ✅ 大幅超额 |
| 容器启动时间 | 预热 BusyBox，串行 CreateContainer + StartContainer P95 为 47.2 ms，同轮 containerd 为 71.8 ms。见[测试口径和数据](PERFORMANCE.md) | < 100 ms (P95)，本轮预热场景达到 |
| 并发操作 | 已测 4 路并发，Nautilus 的创建加启动 P95 为 91.1 ms | 1000+，容量上限待测 |
| 生产加固 | socket 0660、SIGTERM 优雅退出、registry auth、脚手架删除、警告清零 | — ✅ |
| 多沙箱 | runc(libcontainer)+ wasmtime 双后端路由、RuntimeClass 路由验证通过 | — ✅ |
| Wasm 策略引擎 | hooks 热加载 + fuel/timeout 限额,demo 验证通过 | — ✅ |
| Kubernetes 集成 | kubelet 直连未验证(critest/crictl 全链路已绿) | 1.25+ ⏳ |

### 已完成能力(按领域)

- **RuntimeService**:Pod/容器完整生命周期、幂等 stop/remove、
  stats(单容器/list/pod 聚合)、UpdatePodSandboxResources/Checkpoint/Restore
  等新 RPC 显式 unimplemented(对齐上游渐进支持惯例)
- **ImageService**:原生 oci-distribution 拉取(内容寻址层缓存、Basic auth、
  registry mirror)、docker 语义对齐(ID=config digest、tag 归并、untag 删除)
- **网络**:CNI chain(loopback/bridge/host-local)、pod netns holder、
  hostPort 发布、hostNetwork pod 级语义、DNS config
- **安全**:内置 moby seccomp、AppArmor 全合规、caps 五 set、
  noNewPrivileges/readonlyRootfs、privileged 继承、sysctls、socket 0660
- **Streaming**:SPDY exec/attach/portforward 全绿
- **命名空间**:pod 级 net/pid/ipc holder;NamespaceOption
  HOST/POD/CONTAINER/NODE 全语义
- **存储**:rbind/rshared 卷挂载、LogPath 落盘与 rotate reopen、OOMKilled 归因

## 🚀 后续路线

### 近期(稳定性与度量)

1. **性能回归与容量测量**：已发布[与 containerd 的首轮对比和原始样本](PERFORMANCE.md)。
   后续跨时段复测，拆解 CreateContainer 开销，并补充冷镜像、多层镜像、CNI 和阶梯并发测试。
2. **e2e wait4 挂起排查**:`e2e_lifecycle_test` 两次复现阻塞在 wait4
   (疑与 CPU 饥饿同源),干净环境观察定位
3. **debug 日志与流式输出隔离**:libcontainer 内部 DEBUG 行泄入 exec 流
   (生产 INFO 级干净;深挖 tenant 日志路由)
4. **critest 版本跟进**:v1.31.1 → 上游新版本回归(96/0 基线巩固)
5. **kubelet 直连验证**:真实 K8s 节点接入(kind 或裸 kubelet),补齐
   Kubernetes 集成层验证

### 中期(差异化补全)

1. **Wasm 策略 WIT/component 接口**:任意语言写策略(component model),
   替代现有 raw ABI;per-hook 独立 fuel/timeout 策略
2. **镜像签名与验签**:Cosign 外挂集成(设计上明确不自研签名协议栈)
3. **状态存储评估**:sled → redb(sled 已停止维护;接口不变,低优先级)
4. **attach/PTY 真实化**:attach 从"容器内新起 /bin/sh"演进为主进程
   stdio 注入;tty 分配真实 PTY
5. **镜像 GC**:现有 gc 配置项未接实际回收逻辑

### 远期(设计预留)

- **microVM 后端(libkrun)**:需要带 KVM 的宿主机(现有验证机不满足);
  `Sandbox` trait 已预留,RuntimeClass 路由即插即用
- **机密计算(CoCo)**:依赖 microVM 后端先行
- **企业级**:安全审计、兼容性认证、长稳(7x24)测试

## 🧪 质量门控(现行)

每次提交:
- `cargo build` / `cargo test` 绿
- `cargo clippy -- -D warnings`、`cargo fmt --check` 零告警(当前基线)
- 远程验证脚本 `scripts/verify_remote.sh` 断言全过

阶段收敛(以 critest 为准绳):任何新增 CRI 表面行为改动后,全量 critest
不回退(当前 96/0);跑前检查 `/proc/pressure/cpu` 与失控进程,排除环境噪声。
