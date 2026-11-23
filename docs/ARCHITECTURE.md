# Nautilus 架构设计文档

> 更新时间:2026-08-29,对应当前实现(critest 96/0 全绿,MVP 阶段完成)。
> 测试基线见 [CRITEST_RESULTS.md](CRITEST_RESULTS.md),规划见 [DEVELOPMENT_ROADMAP.md](DEVELOPMENT_ROADMAP.md)。

## 1. 项目定位

Nautilus 是 Rust 实现的 Kubernetes CRI 兼容容器运行时,差异化方向是
**多沙箱统一运行时**:单二进制(~10MB 级、空闲 PSS 实测 10 MiB)同时纳管
runc 容器与 WASM 工作负载,生命周期策略用 Wasm 进程内热插拔;microVM
(libkrun)为设计预留。

| 问题 | 回答 |
|---|---|
| 为什么要 Rust? | wasmtime / libcontainer 可进程内嵌入;Go 生态只能靠 shim 子进程拼装 |
| 为什么不用 containerd/CRI-O? | 单二进制多引擎、默认加固、常驻 <10MB,三者无现成方案同时满足 |

## 2. 总体架构

```
   nautilus (docker 风格客户端)          kubelet / crictl / critest
            │ CRI gRPC (UDS)                    │ CRI gRPC (UDS)
            └──────────────┬─────────────────────┘
                           ▼
        /run/nautilus/nautilus.sock (0660, root:root)
   ┌──────────────────────────────────────────────────────────┐
   │                    nautilusd (单进程)                     │
   │  ┌────────────────────────────────────────────────────┐  │
   │  │ CRI gRPC 服务 (tonic/prost, CRI proto 43 RPC)      │  │
   │  │  RuntimeService · ImageService · Version           │  │
   │  │  Streaming server (SPDY: exec/attach/portforward)  │  │
   │  └───────────────┬────────────────────────────────────┘  │
   │  ┌───────────────▼───────────────┐   ┌────────────────┐  │
   │  │  Sandbox 路由 (runtime_handler)│   │  PolicyEngine  │  │
   │  │  ┌────────────┐ ┌───────────┐ │   │  (wasmtime,    │  │
   │  │  │ RuncSandbox│ │WasmSandbox│ │   │   进程内热加载) │  │
   │  │  │(libcontainer)│ (wasmtime) │ │   └────────────────┘  │
   │  │  └────────────┘ └───────────┘ │                       │
   │  └───────────────────────────────┘                       │
   │  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐  │
   │  │ 网络/podnet│ │ 存储/pull │ │ 状态/sled │ │ 安全 (spec)│  │
   │  └──────────┘ └──────────┘ └──────────┘ └────────────┘  │
   └──────────────────────────────────────────────────────────┘
```

### 技术栈(实际依赖)

| 领域 | 选型 | 说明 |
|---|---|---|
| 异步运行时 | tokio 1.x | 全 feature |
| gRPC | tonic 0.12 + prost 0.13 | UDS 监听 |
| OCI 容器引擎 | **libcontainer 0.7**(youki 拆出的库) | 进程内调用;`[patch.crates-io]` 指向 Bevisy/youki fork rev `2b4cdda`(= 2867015 + rootfs 递归传播修复);可回退 exec-runc |
| WASM | wasmtime 26 + wasmtime-wasi 26 | 沙箱执行 + 策略引擎复用 |
| 镜像分发 | oci-distribution 0.11(rustls) | 原生拉取,零 docker CLI 依赖 |
| 流式协议 | spdystream-rs 0.1 | SPDY/3.1 remotecommand v4 + portforward |
| 状态存储 | sled 0.34 | PodState/ContainerState 持久化 |
| OCI 类型 | oci-spec 0.10 | spec 生成 |
| 安全 | nix / libc / libseccomp | ns、mount、cap、seccomp |

## 3. 源码结构

```
src/
├── main.rs               # nautilusd 入口:参数、配置、服务装配、信号处理
├── bin/nautilus.rs       # docker 风格客户端(经 CRI gRPC 操作 daemon)
├── core/
│   ├── config.rs         # TOML 配置(serde;未知节忽略,向后兼容)
│   ├── error.rs          # thiserror 错误体系
│   └── state.rs          # sled 状态持久化
├── cri/
│   ├── runtime_service.rs# RuntimeService:Pod/容器生命周期编排
│   ├── image_service.rs  # ImageService:pull/list/remove/status
│   ├── streaming.rs      # SPDY 流式 server(exec/attach/portforward)
│   └── version.rs        # Version RPC
├── sandbox/
│   ├── mod.rs            # Sandbox trait + ResourceLimits 等抽象
│   ├── runc_backend.rs   # libcontainer 进程内实现(exec-runc 兜底)
│   ├── wasm_backend.rs   # wasmtime WASI 实现
│   └── spec.rs           # CRI 安全上下文 → OCI spec(seccomp/AppArmor/caps)
├── runtime/
│   ├── pod.rs            # PodState(pod 级 netns/pidns/ipcns 持有者语义)
│   └── container.rs      # ContainerState / WorkloadSpec
├── network/
│   ├── podnet.rs         # pod netns holder(fork + unshare + bind-mount)
│   ├── cni.rs            # CNI chain 执行器(stdin JSON / stdout Result)
│   ├── bridge.rs         # nautilus0 网桥
│   ├── publish.rs        # hostPort 发布(loopback forwarder)
│   └── sysctl.rs         # sysctl 作用域路由
├── storage/
│   ├── pull.rs           # 原生 registry 拉取(内容寻址层缓存、Basic auth)
│   ├── unpack.rs         # layer 解包(whiteout/opaque、路径穿越防护)
│   └── image.rs          # 镜像记录(tags/repo digests 归并、引用定位)
├── policy/mod.rs         # Wasm 策略引擎(hooks、fuel、timeout、热加载)
└── proto/                # CRI protobuf 生成代码(k8s master,43 RPC)
```

## 4. 关键抽象与流程

### 4.1 Sandbox trait(多后端路由)

```rust
// src/sandbox/mod.rs
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxHandle>;
    async fn start(&self, h: &SandboxHandle) -> Result<()>;
    async fn stop(&self, h: &SandboxHandle, force: bool) -> Result<()>;
    async fn stats(&self, h: &SandboxHandle) -> Result<SandboxStats>;
    fn handles(&self, class: &str) -> bool;   // RuntimeClass 匹配
}
```

- `runtime_service.rs` 面向该 trait 编排;后端按 CRI
  `RunPodSandboxRequest.runtime_handler` 路由(未知 handler 按 CRI 规范拒绝)。
- K8s 用户侧声明 `runtimeClassName: wasi` 即路由到 wasm 引擎;config
  `[sandboxes]` 段控制 `default_handler` / `wasm_enabled`。
- PodState 记 `runtime_handler`、ContainerState 记 `sandbox_kind`
  (serde default 向后兼容旧状态文件)。

### 4.2 runc 后端(libcontainer 进程内)

- 默认 `ContainerBuilder::as_init().build()` + `start/kill/delete`,
  fork 在 `spawn_blocking` 执行;`runtime.runc.driver = "exec"` 回退子进程方案。
- 工作负载**直 exec**(无 shell wrapper):容器 init/tenant 退出码诚实、pid1 语义正确。
- CRI LinuxContainerResources → `ResourceLimits` → OCI `linux.resources`
  (cpu shares/quota/period/cpuset/memory,仅写非零约束)。
- stdio:**pipe2(O_CLOEXEC) + 阻塞线程读写 + EINTR 重试**。
  (socketpair 无法经 /proc/self/fd 重开;AsyncFd+fork 会冻结 gRPC accept loop)
- OOM 归因:start 时记录 system.slice oom_kill 基线 + 30ms 轮询 scope
  memory.events + `oom_killed()` 兜底;exit 137 → reason=OOMKilled。
- 日志:CRI LogPath 标准落盘(RFC3339 行格式、append、rotate 后 reopen)。

### 4.3 pod 级命名空间持有者

RunPodSandbox 时创建常驻 holder 子进程(均 fork → unshare → fork 孙进程,
creator 写握手 pipe 后退出;孙进程 park 保持 ns 存活;holder 会
`close_out_inherited_fds()` 防 fd 泄漏):

| ns | holder | 用途 |
|---|---|---|
| net | unshare(CLONE_NEWNET),bind-mount `/proc/<pid>/ns/net` → netns 目录 | CNI 挂网;同 pod 容器共享 |
| pid | unshare(CLONE_NEWPID),bind-mount ns/pid | 容器 spec `__POD_PIDNS__` 占位替换;pid=POD 共享 |
| ipc | unshare(CLONE_NEWIPC) | pod 级 IPC ns;sysctls 路由 |

NamespaceOption:HOST(省略 ns 类型)/ POD / CONTAINER / NODE 各自落位;
hostNetwork 为 pod 级字段,容器强制继承(跳过 netns/CNI/port-publish)。

### 4.4 网络

- CNI 执行器:按 chain 顺序 exec 插件;conflist 顶层 name/cniVersion 注入;
  loopback 用 `CNI_IFNAME=lo`,其余 eth0;ADD 结果 IP 写入
  PodSandboxStatus.network.ip;Stop 级联 DEL + 杀 holder + umount(MNT_DETACH)。
- conf_dir 为空时生成默认 `10-nautilus.conflist`(loopback+bridge+host-local,
  ipMasq=false 避免 iptables 依赖)。
- hostPort:loopback forwarder(沙箱与流式路径共享 helper)。

### 4.5 镜像(原生分发,零 docker 依赖)

- `pull.rs`:oci-distribution rustls 客户端;sha256 流式校验;层缓存
  `<image_dir>/layers/<digest>.blob`;`storage.pull_mode = "native"(默认)| "cli"`;
  `storage.registry_mirror` 支持 docker.io 重写;代理走 HTTPS_PROXY。
- 镜像 config(entrypoint/cmd/env/workdir)从 registry 取 typed ConfigFile。
- 认证:CRI AuthConfig(username/password + base64 auth)→ Basic 认证。
- 语义:Image.ID = config digest;size = manifest 层字节;同 digest 多 tag
  归并单记录;remove 为 docker untag 语义;引用定位器支持精确键/tag/digest/ID 前缀。
- manifest+config 本地缓存 → 已缓存镜像的 create 完全离线。

### 4.6 流式 server(exec / attach / portforward)

`src/cri/streaming.rs` + spdystream-rs,SPDY/3.1 remotecommand v4.channel.k8s.io
+ portforward.k8s.io;三个 CRI RPC 返回 `http://127.0.0.1:{port}/{mode}/{token}`,
监听 127.0.0.1:0 懒启动。

协议要点(对实现有决定性影响):
- critest 不回传查询参数 → **会话参数在 CRI RPC 时全部注册进 token**
- 通道靠 SYN_STREAM 的 `streamType` 头区分(error/stdin/stdout/stderr/resize);
  portforward 为 data/error + port + requestID 头
- 101 响应必须回显 `X-Stream-Protocol-Version`,否则客户端拒绝
- Go 客户端 SPDY 请求流不带 `:path` 伪头 → 用 parse_upgrade_request +
  Connection::serve 自行拼装解析路径
- 退出码走 error 流 metav1.Status JSON(空 EOF = 成功);v1 portforward 纯字节中继
- 后端 `SandboxBackend::exec_stream`:libcontainer tenant + socketpair stdio,
  WNOHANG reap;断连 SIGKILL 防泄漏

已知限制:attach 为容器内新起 `/bin/sh`(主进程 stdio 走文件);tty 无真实 PTY;
debug 日志级别下 libcontainer 内部行可能泄入 exec 流输出(生产 INFO 干净)。

### 4.7 安全

- **安全规格模块 `sandbox::spec`**(TDD,10 单测):CRI 安全上下文 → OCI spec。
  runtime/default 内置 seccomp(moby 维护 profile,剥离非 OCI 字段 +
  aarch64 flat architectures——libcontainer 不认 OCI archMap);
  localhost/<name> kubelet 布局解析(防穿越);绝对路径直读。
- AppArmor:宿主 loader 集成,per-container profile 应用全合规。
- caps:CAP_ 前缀规范化,adds 进全部五个 capability set;privileged
  沙箱透传给无 security_context 的容器(全 caps)。
- noNewPrivileges/readonly rootfs:配置默认 + CRI 请求级覆盖
  (LinuxContainerSecurityContext → ContainerState → WorkloadSpec → OCI spec)。
- 基线伪文件系统:/proc procfs、/dev tmpfs、/sys sysfs ro。
- **CRI socket 0660**(root:root,对齐 containerd/CRI-O 惯例——0755 等于
  本地任意用户可起特权容器)。
- sysctls:Net/Ipc/Other 作用域路由。
- 幂等语义:Stop/Remove Pod/Container 对缺失或已停资源返回成功
  (kubelet 清理路径依赖)。

### 4.8 Wasm 策略引擎

`src/policy/mod.rs`,进程内 wasmtime 评估,无 sidecar 无 IPC:

- 策略 ABI:导出 `memory`、`alloc(len)->ptr`、`evaluate(ptr,len)->i32`
  (0=放行),可选 `reason_ptr/reason_len` 回传拒绝原因
- 钩子点:run_pod_sandbox / create_container / start_container(`[policy].hooks`)
- Module 按 (mtime,size) 热加载,替换文件即生效
- fail-open 默认,`fail_closed` 可切;`fuel_limit`(默认 10M)+ `timeout_ms`
  (默认 100)双保险,策略死循环不拖垮运行时
- demo:`examples/policy-deny-label`,no_std wasm32 模块 383 字节,按 label 拒绝
- 后续方向:WIT/component 接口(任意语言写策略)

### 4.9 状态与配置

- sled 持久化 PodState/ContainerState;新增字段均 serde default,旧状态文件兼容。
- 配置层次:默认值 → TOML 文件;未知节/字段忽略(旧配置向后兼容)。

主要配置节(详见 `src/core/config.rs`):

| 节 | 要点 |
|---|---|
| `[runtime]` | state_dir、log_level、worker_threads、`[runtime.runc] driver`(libcontainer/exec)、`[runtime.sandboxes]`(default_handler、wasm_enabled) |
| `[storage]` | image_dir/container_dir、`pull_mode`(native/cli)、`registry_mirror`、`image_cli`(cli 兜底) |
| `[network]` | CNI conf/bin 目录、netns_dir、bridge_name/subnet/gateway、dns、firewall |
| `[cri]` | listen_address、消息大小限制、request/stream 超时 |
| `[security]` | no_new_privs(默认 true)、read_only_rootfs、seccomp_profile_path |
| `[policy]` | enabled、path、hooks、fail_closed、fuel_limit、timeout_ms |
| `[registry]/[auth]/[limits]/[proxy]/[debug]` | registry 认证、资源限额、代理、调试 |

## 5. 生产就绪项(已落地)

- SIGINT + SIGTERM 共用优雅退出路径(systemd stop 走 SIGTERM);socket 文件清理
- PullImage 凭据贯穿(私有镜像可用,错误凭据被 registry 401 拒绝)
- 零编译警告;脚手架空壳已删(monitoring/performance/experimental/plugins/
  security、conmon/runc 封装共 ~2900 行)
- grpc-timeout 三层强制(预算下限 + tokio 定时器 + Instant 回检)
- exec_sync 超时下沉 backend(WNOHANG + SIGKILL + reap,不泄漏进程)
- 资源统计:container_stats / list_container_stats / pod_sandbox_stats
  (/proc 进程级计账);`scripts/measure_memory.sh` 内存测量

## 6. 验证体系

| 层 | 工具/脚本 | 现状 |
|---|---|---|
| 单元/集成 | `cargo test`(6 套件 28 用例 + e2e 5) | 绿(e2e 偶发 wait4 挂起,待查) |
| CRI 一致性 | critest v1.31.1(`scripts/critest_runner.sh` 等) | **96/0 全绿** |
| 端到端冒烟 | `scripts/verify_remote.sh`(13 断言)、`smoke_ctl.sh`/`smoke_workloads.sh` | ALL-P0-PASS |
| 策略 demo | `examples/policy-deny-label` | POLICY-DENY-PASS |

在 Linux 上可直接运行 `cargo build`。远程构建时，为 `scripts/verify_remote.sh`
设置 `NAUTILUS_REMOTE=<host>`，脚本会按目标机器的架构选择构建目标。
运维注意:跑 critest 前查 `/proc/pressure/cpu` 与失控进程(CPU 饥饿会造成
随机挂起,与代码无关);kill -9 测试进程会泄漏容器 workload。
