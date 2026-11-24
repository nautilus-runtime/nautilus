# Pod VM 的 OCI guest 协议 v1

每个 Pod attempt 使用独立的 vsock 和 `podfs` 导出。guest PID 1 在 4082 上提供启动身份握手。镜像包含 workload agent 时，agent 在挂载 podfs、cgroup v2 并运行 `/bin/runc --version` 成功后监听 4083。两个端口只接受源 CID 为 2 的连接。宿主通过已核实归属的 Cloud Hypervisor Unix socket 连接目标 guest。

## 连接

1. 宿主写入 ASCII `CONNECT 4083\n`，读完 Cloud Hypervisor 的 `OK <port>\n`。
2. guest 先发 hello。每个帧均为 **4 字节大端无符号长度 + 对应字节数的 UTF-8 JSON**。hello 为 `{"version":1,"sandboxId":"…","nonce":"…","bootId":"…"}`。宿主逐项核对自己的该次启动记录，再发送命令。绑定到同一连接可以在向对端发送 nonce 前识别错误端点。
3. 宿主发送一个请求，保持连接的写端打开，读取一个应答。每个连接执行一次调用。host 读帧时必须限制应答长度在 1 MiB 内，同时校验协议版本、请求 ID 和 `ok`。guest 限制请求为 1–65536 字节，接收预算 3 秒。错误帧长和传输截断会关闭连接。
4. 请求包含 hello 的四个字段，以及 `requestId`、`containerId`、`op`。四种 ID 均为小写、带连字符的非零 UUID。`requestId` 由宿主为每次调用生成。VM 重启后，旧 bootId 失效。宿主可关闭 exec 连接取消执行。

| op | 额外字段 | 成功应答 |
| --- | --- | --- |
| `create` | 无 | 从本 Pod 导出的固定位置读取 bundle，执行 runc create，返回状态。 |
| `start` | 无 | 启动已经创建的 init 进程，返回状态。对于已经启动过的容器，重试返回当前状态。 |
| `status` | 无 | 返回 `state`，取值 `created`、`running` 或 `exited`。 |
| `exec` | `argv` 非空数组，最多 256 项。`timeoutMs` 为 0–300000 的整数。0 表示执行无时间上限。 | 返回 `stdout`、`stderr` 和 `exitCode`。两个流独立用标准带填充 Base64 编码，空流可省略。每个流最多 256 KiB，超限返回错误。 |
| `stop` | `timeoutMs` 可省略，取值 0–300000。 | 返回退出状态。0 或省略表示立即 SIGKILL，正值表示先 SIGTERM 并允许指定毫秒数的退出时间，超时后 SIGKILL。目标已删除时成功。 |
| `readLog` | `offset` 非负有符号 64 位整数，`limit` 为 1–262144。 | `stdout` 为从 offset 开始的 CRI 日志字节的 Base64（空时可省略），`nextOffset` 为 offset 加实际读取字节数。可在进程运行时轮询和重试。 |
| `remove` | 无 | 结束进程、删除 guest 中的 runc 实例、释放私有运行目录。目标不存在时成功。宿主在同步日志且 remove 成功后负责清理导出的 bundle。 |

状态应答具有 `createdAt`、`startedAt`、`finishedAt`，单位为 Unix epoch 纳秒，尚未发生的时间可省略。`exitCode` 在退出状态或 exec 结果中有效；信号退出取 128 加信号编号。guest 作为 subreaper 对容器 init 执行 wait，退出状态发布前会排空日志管道。宿主根据时间和状态填写 CRI 字段。

成功应答具有 `{"version":1,"requestId":"…","ok":true,...}`。失败时 `ok=false`，`code` 取 `invalid`（协议、参数或 bundle）、`identity`、`not_found`、`conflict`（不符合当前生命周期）、`timeout`、`limit` 或 `runtime`，`message` 用于诊断。客户端首先检查 `ok`。格式损坏的请求可能没有可回显的 requestId。传输失败时 create/start/stop/remove 可对同一容器重新调用；exec 可能已经产生工作负载副作用，宿主须将断连报告给调用方。agent 以每次调用的 cgroup 回收 exec，覆盖超时、断连及脱离原进程组的后代。其他容器和目标容器的 init 继续运行。最多 32 个连接并发，其中最多 24 个执行 exec。

## OCI 交付

宿主在独占 podfs 的 `containers/<containerId>/` 原子发布完整的 `config.json` 和 `rootfs/`，待 guest remove 成功才回收。agent 将导出挂在 `/run/nautilus/share`，从固定位置读取，随后把审核后的 spec 写入仅 guest 可见的 bundle，调用静态 runc。宿主必须保证各容器使用独立可写 rootfs，授权卷和 bundle 在其使用期内不可替换。工作负载只能访问自己的 rootfs 和显式挂载的卷。

- `ociVersion` 支持 1.0.2、1.1.0、1.2.0。`root.path` 必须是 `rootfs`，接受 `root.readonly`。配置不超过 256 KiB。
- `process` 接受 `args`、`env`、规范的绝对 `cwd`、数字 `user.uid/gid/additionalGids`。要求 `terminal=false` 和 `noNewPrivileges=true`，能力集仅接受空集合。exec 复用该进程的环境、安全设置、用户和目录。guest 拒绝未实现的配置字段。
- `linux.namespaces` 必须包含 `mount` 和 `pid`。每种类型最多一次，只接受 `{"type":"…"}`。默认共享 Pod guest 的 network、IPC、UTS；增加 `ipc` 或 `uts` 可请求容器私有的对应 namespace。Pod IPC 共享 `/dev/shm`，私有 IPC 使用单独的 tmpfs。`linux.resources` 支持 `cpu.{shares,quota,period}` 与 `memory.limit`，由 runc 写入该容器自己的 cgroup。
- agent 提供 proc、私有 dev tmpfs、shm 和 mqueue，屏蔽敏感 proc 节点并将内核参数只读挂载，设备访问使用拒绝优先的规则。guest 中配置了 DNS 时，容器默认使用 guest 的只读 `/etc/resolv.conf`。
- `mounts[]` 只接受 `{"type":"bind","source":"volumes/<UUID>","destination":"/absolute/path","options":["bind","ro"]}` 或将最后一项替换成 `rw`。来源相对于该 Pod 的导出根，由宿主按授权准备；agent 要求来源由 root 所有，拒绝符号链接、特殊文件及多硬链接文件，目标必须处于容器 rootfs 内。挂载附加 nosuid/nodev/noexec。guest 拒绝覆盖根目录、/dev、/sys 或 /proc 的请求。宿主需在接受 CRI mount 前确认数据来源、读写和传播语义能够兑现。
- 容器 init 的日志独立写入该 bundle 的 `container.log`，格式为 `RFC3339Nano stdout|stderr F|P payload\n`。16 KiB 超长行分片使用 `P`。guest 使用独占创建的文件描述符追加与读取，readLog 无路径参数。宿主按 offset 复制到 kubelet 的日志文件。guest 的日志保存在本次 Pod 导出中，生命周期与该 bundle 绑定。

## 构建与 KVM 验证

原生 amd64 或 arm64 Linux 上使用 Go 1.24 或更高版本、静态 C 工具链、GNU cpio、curl、kmod、gzip 和目标 guest kernel 的模块。脚本校验 runc v1.5.3 上游发布二进制的 SHA-256。离线时可通过 `GUEST_RUNC_BIN` 提供相同文件，并预先填充 Go 模块缓存；也可用 `GUEST_AGENT_BIN` 提供从本目录交叉编译的静态程序。同一工具链、二进制与 kernel modules 生成一致的归档。

```sh
./guest/build-workload-initramfs.sh /tmp/agent.cpio.gz "$(uname -r)"
sudo env VMM=/path/to/cloud-hypervisor VIRTIOFSD=/path/to/virtiofsd \
    KERNEL="/boot/vmlinuz-$(uname -r)" INITRAMFS=/tmp/agent.cpio.gz \
    BUSYBOX=/path/to/static-busybox ./guest/tests/boot.sh
```

2026-11-24 在 Linux x86_64 5.15.0-191-generic、Cloud Hypervisor v54.1、virtiofsd 1.13.2、runc 1.5.3 上运行该用例。协议实测涵盖同一 boot ID、两个容器的独立 PID 与可写根、Pod IPC、十次 exec 的工作区、日志分流与偏移、授权只读卷、退出码与纳秒时间、参数和 bundle 拒绝、输出上限、超时与断连后的后代清理、重复 stop/remove。宿主 CRI 及 kubelet 由其自身入口另行验收。
