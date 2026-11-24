# Pod VM 的 OCI guest 协议 v1

每个 Pod attempt 使用独立的 vsock 和 `podfs` 导出。guest PID 1 在 4082 上提供启动身份握手。镜像包含 workload agent 时，agent 在挂载 podfs、cgroup v2 并运行 `/bin/runc --version` 成功后监听 4083。两个端口只接受源 CID 为 2 的连接。宿主通过已核实归属的 Cloud Hypervisor Unix socket 连接目标 guest。

## 连接

1. 宿主写入 ASCII `CONNECT 4083\n`，读完 Cloud Hypervisor 的 `OK <port>\n`。
2. guest 先发 hello。每个帧均为 **4 字节大端无符号长度 + 对应字节数的 UTF-8 JSON**。hello 为 `{"version":1,"sandboxId":"…","nonce":"…","bootId":"…"}`。宿主逐项核对自己的该次启动记录，再发送命令。绑定到同一连接可以在向对端发送 nonce 前识别错误端点。
3. 宿主发送一个请求，保持连接的写端打开，读取一个应答。每个连接执行一次调用。host 读帧时必须限制应答长度在 1 MiB 内，同时校验协议版本、请求 ID 和 `ok`。guest 限制请求为 1–65536 字节，接收预算 3 秒。错误帧长和传输截断会关闭连接。
4. 请求包含 hello 的四个字段、`requestId` 和 `op`。容器操作还包含 `containerId`，`podStats` 省略它。四种 ID 均为小写、带连字符的非零 UUID。`requestId` 由宿主为每次调用生成。VM 重启后，旧 bootId 失效。宿主可关闭 exec 连接取消执行。

| op | 额外字段 | 成功应答 |
| --- | --- | --- |
| `create` | 无 | 从本 Pod 导出的固定位置读取 bundle，执行 runc create，返回状态。 |
| `start` | 无 | 启动已经创建的 init 进程，返回状态。对于已经启动过的容器，重试返回当前状态。 |
| `status` | 无 | 返回 `state`，取值 `created`、`running` 或 `exited`。 |
| `stats` | 无 | 返回该运行中容器 cgroup 的 `stats`。 |
| `podStats` | 无 | 返回工作负载父 cgroup 的 `stats` 和以运行中容器 ID 为键的 `containers` 采样表。无运行中容器时可省略表。 |
| `exec` | `argv` 非空数组，最多 256 项。`timeoutMs` 为 0–300000 的整数。0 表示执行无时间上限。 | 返回 `stdout`、`stderr` 和 `exitCode`。两个流独立用标准带填充 Base64 编码，空流可省略。每个流最多 256 KiB，超限返回错误。 |
| `stop` | `timeoutMs` 可省略，取值 0–300000。 | 返回退出状态。0 或省略表示立即 SIGKILL，正值表示先 SIGTERM 并允许指定毫秒数的退出时间，超时后 SIGKILL。目标已删除时成功。 |
| `readLog` | `offset` 非负有符号 64 位整数，`limit` 为 1–262144。 | `stdout` 为从 offset 开始的 CRI 日志字节的 Base64（空时可省略），`nextOffset` 为 offset 加实际读取字节数。可在进程运行时轮询和重试。 |
| `remove` | 无 | 结束进程、删除 guest 中的 runc 实例、释放私有运行目录。目标不存在时成功。宿主在同步日志且 remove 成功后负责清理导出的 bundle。 |

统计样本的 `timestamp` 为 Unix epoch 纳秒。必填的无符号计数为 `cpuUsageNanos`、`memoryUsageBytes`、`workingSetBytes`、`rssBytes`、`pageFaults` 和 `majorPageFaults`。两次独立采样间隔达到一秒后可提供 `usageNanoCores`，cgroup 有有限的内存上限时提供 `availableBytes`。采样失败返回错误。宿主按样本自身的时间戳填充 CRI 字段。[统计语义](VM_RUNTIME.md#guest-工作负载统计)

状态应答具有 `createdAt`、`startedAt`、`finishedAt`，单位为 Unix epoch 纳秒，尚未发生的时间可省略。`exitCode` 在退出状态或 exec 结果中有效；信号退出取 128 加信号编号。guest 作为 subreaper 对容器 init 执行 wait，退出状态发布前会排空日志管道。宿主根据时间和状态填写 CRI 字段。

成功应答具有 `{"version":1,"requestId":"…","ok":true,...}`。失败时 `ok=false`，`code` 取 `invalid`（协议、参数或 bundle）、`identity`、`not_found`、`conflict`（不符合当前生命周期）、`timeout`、`limit` 或 `runtime`，`message` 用于诊断。客户端首先检查 `ok`。格式损坏的请求可能没有可回显的 requestId。传输失败时 create/start/stop/remove 可对同一容器重新调用；exec 可能已经产生工作负载副作用，宿主须将断连报告给调用方。agent 以每次调用的 cgroup 回收 exec，覆盖超时、断连及脱离原进程组的后代。其他容器和目标容器的 init 继续运行。最多 32 个连接并发，其中最多 24 个执行 exec 或使用交互流。

创建期间 runc 或其清理命令失败时，guest 返回错误。仍有待回收对象的 ID 将保留在 guest 的启动记录中，宿主须重试 remove 或 stop，待成功后再重新创建。

## 交互流

4083 的单次请求可以升级为双向流。握手与启动身份校验和其他调用一致。guest 在目标处于运行态且完成 I/O 准备后返回带原 requestId 的 JSON 成功应答，随后双方在同一连接使用二进制帧。建立失败返回常规 JSON 错误。每个转发 TCP 连接使用独立的 4083 会话。

| 操作 | containerId | 参数 |
| --- | --- | --- |
| `execStream` | 容器 ID | `argv` 和 `stream`。创建新的 exec 进程，可选择分配 PTY。 |
| `attach` | 容器 ID | `stream`。连接到启动时保存的原 init 的输入输出，接收从 attach 时刻开始的输出。TTY 选项须与 init 一致。 |
| `portForward` | sandboxId | `stream: {"port": 8080}`。在 Pod guest 内连接 TCP 端口 1–65535，先尝试 IPv4 loopback，再尝试 IPv6 loopback。 |

exec 和 attach 的 `stream` 带有可选布尔值 `stdin`、`stdout`、`stderr`、`tty`，缺省为 false。至少启用一个标准流。TTY 将设备输出合并到 stdout，要求 stderr 为 false。OCI bundle 的 `annotations` 可指定 `io.nautilus.stdin` 和 `io.nautilus.stdinOnce`，值为字符串 `"true"` 或 `"false"`，缺省为 false。stdinOnce 需要打开 stdin。普通 attach 断连后，后续连接还能向原进程写入。启用 stdinOnce 时，首次带输入的 attach 结束会关闭原进程的 stdin。

每个二进制帧由 1 字节通道、4 字节大端无符号长度和 payload 组成。payload 最多 65536 字节。流持续期间每个方向都可独立发送。

| 通道 | 方向 | 负载 |
| --- | --- | --- |
| 0 | 宿主至 guest | 原始 stdin 或 TCP 数据 |
| 1 | guest 至宿主 | 原始 stdout、PTY 输出或 TCP 数据 |
| 2 | guest 至宿主 | 原始 stderr |
| 3 | guest 至宿主 | UTF-8 会话错误，随后关闭连接 |
| 4 | 宿主至 guest | PTY resize，两个大端 u16，依次为宽和高 |
| 5 | 宿主至 guest | 零长度，此会话停止发送输入。exec 关闭 stdin，TTY 写入 EOT，TCP 半关闭写方向 |
| 6 | guest 至宿主 | 大端有符号 i32 退出码。TCP 对端正常结束时返回 0 |

整个传输断连会取消这次 exec 及其 cgroup 内后代，portForward 关闭对应 TCP 连接，attach 仅移除本次订阅。guest subreaper 等待 runc 启动的具体进程，排空输出后发送退出码。每个 attach 有独立的有限输出队列，超限时报错断开，该容器会继续写 CRI 日志。终端由 runc 在容器私有 devpts 中分配，guest 通过 console socket 接收并保管 PTY master，窗口更改使用 TIOCSWINSZ。

进程生命周期、标准流、关闭输入以及调整窗口的分工沿用 Kata agent 协议。Nautilus 将这些动作复用在单条已核实 Pod 身份的双向连接上，宿主据此桥接 kubelet 的流。宿主通过 Unix socket 发起 CONNECT，guest 通过 AF_VSOCK 接收的通路与 Firecracker 描述的 virtio-vsock 模型一致，Pod 网络仅用于实际负载流量。实现分别使用 Cloud Hypervisor 与 runc。[Kata agent protocol](https://github.com/kata-containers/kata-containers/blob/main/src/libs/protocols/protos/agent.proto) · [Firecracker virtio-vsock](https://github.com/firecracker-microvm/firecracker/blob/main/docs/vsock.md) · [runc v1.5.3 console handling](https://github.com/opencontainers/runc/blob/v1.5.3/utils_linux.go)

## OCI 交付

宿主在独占 podfs 的 `containers/<containerId>/` 原子发布完整的 `config.json` 和 `rootfs/`，待 guest remove 成功才回收。agent 将导出挂在 `/run/nautilus/share`，从固定位置读取，随后把审核后的 spec 写入仅 guest 可见的 bundle，调用静态 runc。宿主必须保证各容器使用独立可写 rootfs，授权卷和 bundle 在其使用期内不可替换。工作负载只能访问自己的 rootfs 和显式挂载的卷。

- `ociVersion` 支持 1.0.2、1.1.0、1.2.0。`root.path` 必须是 `rootfs`，接受 `root.readonly`。配置不超过 256 KiB。
- `process` 接受 `args`、`env`、规范的绝对 `cwd`、数字 `user.uid/gid/additionalGids`。`terminal=true` 分配真实 PTY，要求 `noNewPrivileges=true`，能力集仅接受空集合。exec 复用该进程的环境、安全设置、用户和目录。guest 拒绝未实现的配置字段。
- `linux.namespaces` 必须包含 `mount` 和 `pid`。每种类型最多一次，只接受 `{"type":"…"}`。默认共享 Pod guest 的 network、IPC、UTS；增加 `ipc` 或 `uts` 可请求容器私有的对应 namespace。Pod IPC 共享 `/dev/shm`，私有 IPC 使用单独的 tmpfs。`linux.resources` 支持 `cpu.{shares,quota,period}` 与 `memory.limit`，由 runc 写入该容器自己的 cgroup。
- agent 提供 proc、私有 dev tmpfs、shm 和 mqueue，屏蔽敏感 proc 节点并将内核参数只读挂载，设备访问使用拒绝优先的规则。guest 中配置了 DNS 时，容器默认使用 guest 的只读 `/etc/resolv.conf`。
- `mounts[]` 只接受 `{"type":"bind","source":"volumes/<UUID>","destination":"/absolute/path","options":["bind","ro"]}` 或将最后一项替换成 `rw`。来源是宿主为本容器在该 Pod 的只读卷目录中创建的不透明标识。agent 拒绝选择器中的符号链接和特殊文件，保留获授权文件的 UID/GID 与硬链接语义。目标必须处于容器 rootfs 内。挂载附加 nosuid/nodev/noexec。guest 拒绝覆盖根目录、/dev、/sys 或 /proc 的请求。宿主需在接受 CRI mount 前确认数据来源、读写和传播语义能够兑现。
- 容器 init 的日志独立写入该 bundle 的 `container.log`，格式为 `RFC3339Nano stdout|stderr F|P payload\n`。16 KiB 超长行分片使用 `P`。guest 使用独占创建的文件描述符追加与读取，readLog 无路径参数。宿主按 offset 复制到 kubelet 的日志文件。guest 的日志保存在本次 Pod 导出中，生命周期与该 bundle 绑定。

宿主 SPDY 接入使用锁定的 spdystream-rs 0.1.2。Nautilus 在本地副本中为单条转发区分输入半关闭和 RST，并在接收队列满时等待消费。真实 crictl 回归将 3 MiB 二进制输入经 guest 的 cat 原样读回，逐字节比对。[上游源代码](https://github.com/pelagos-containers/spdystream-rs) · [随项目维护的改动](../vendor/spdystream-rs/src/connection.rs)

## 构建与 KVM 验证

原生 amd64 或 arm64 Linux 上使用 Go 1.24 或更高版本、静态 C 工具链、GNU cpio、curl、kmod、gzip 和目标 guest kernel 的模块。脚本校验 runc v1.5.3 上游发布二进制的 SHA-256。离线时可通过 `GUEST_RUNC_BIN` 提供相同文件，并预先填充 Go 模块缓存；也可用 `GUEST_AGENT_BIN` 提供从本目录交叉编译的静态程序。同一工具链、二进制与 kernel modules 生成一致的归档。

```sh
./guest/build-workload-initramfs.sh /tmp/agent.cpio.gz "$(uname -r)"
sudo env VMM=/path/to/cloud-hypervisor VIRTIOFSD=/path/to/virtiofsd \
    KERNEL="/boot/vmlinuz-$(uname -r)" INITRAMFS=/tmp/agent.cpio.gz \
    BUSYBOX=/path/to/static-busybox ./guest/tests/boot.sh
```

2026-11-24 在 Linux x86_64 5.15.0-191-generic、Cloud Hypervisor v54.1、virtiofsd 1.13.2、runc 1.5.3 上运行该用例。协议实测涵盖同一 boot ID、两个容器的独立 PID 与可写根、Pod IPC、十次 exec 的工作区、日志分流与偏移、授权只读卷、退出码与纳秒时间、参数和 bundle 拒绝、输出上限、超时与断连后的后代清理、重复 stop/remove。在上面的测试命令中设置 `INJECT_CREATE_FAILURE=1` 会构建额外的故障测试镜像，模拟 runc create 已产生 init 后报错、首次清理失败，并验证再次 remove 和同一 ID 的后续创建。宿主 CRI 及 kubelet 由其自身入口另行验收。
