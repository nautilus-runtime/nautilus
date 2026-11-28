# Pod 级虚机运行时

设计跟踪于 [#1](https://github.com/nautilus-runtime/nautilus/issues/1)。目标是在 Kubernetes 中为工作负载选择 `RuntimeClass.handler: vm`，让一次 PodSandbox attempt 拥有一台 Linux VM。同一沙箱内的容器在该 guest 中运行，重复 exec 和工具调用沿用原容器的工作区。显式启用实验 VM 的节点支持通过 CRI 创建 Pod、运行 guest OCI 容器、执行 ExecSync 及恢复日志和网络。交互式 Exec、Attach 和 PortForward 连接到目标 guest。启用并通过探测的节点会向 kubelet 发布 `vm` handler，受信任的宿主文件和目录可挂载到对应的 guest 容器。[节点启用与真实 kubelet 验收](VM_RUNTIMECLASS.md) [CRI v1 契约与本地差异](CRI_VM_CONTRACT.md)

## 所有权与协议

| 边界 | 职责 |
| --- | --- |
| kubelet → nautilusd | 使用标准 CRI v1。以 sandbox ID 寻址每次 attempt，以 container ID 寻址容器，保持默认 OCI/WASI 路由。 |
| nautilusd → Cloud Hypervisor | 每个 sandbox 拥有独立的 VMM 进程、控制 socket、vsock 端点和可写存储。VMM 引导、停止、资源配置与恢复由宿主管理。 |
| nautilusd → Nautilus guest agent | 通过 vsock 进行协议版本和能力协商。团队选择自研 guest agent，负责 guest 内的 OCI 容器、进程、挂载、日志及 exec，可在 guest 内复用 runc。Kata 的 Pod/shim 分工可用作参考。 |
| guest → 工作负载 | 不同容器持有独立 rootfs 和容器级资源约束，按 CRI 的 namespace、卷和 Pod 网络语义共享相应资源。工作负载无需访问 VMM 控制 socket。 |

生产协议按 sandbox、container 和请求 ID 寻址。对创建与删除等可重试操作保存结果；流式操作区分 stdout、stderr、结束状态和取消。guest 启动时校验宿主协商的协议与能力。设计需覆盖消息长度、请求超时、重连、并发以及消息与目标 VM 的绑定。节点探针的单次固定握手只用于检查宿主、内核和传输链路。[Kata Pod 模型](https://github.com/kata-containers/kata-containers/blob/4.4.0/docs/design/architecture_4.0/architecture.md) · [Cloud Hypervisor vsock](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v55.0/docs/vsock.md)

### guest 启动身份

`guest/build-initramfs.sh OUTPUT [KERNEL_RELEASE]` 构建自研 guest agent 的 initramfs。该程序作为 PID 1，在端口 4082 接受来自宿主 CID 2 的 `IDENTIFY 1` 请求。它从内核命令行读取 sandbox ID 与每次启动的 nonce，从 guest 的 procfs 读取 boot ID。宿主核对 sandbox ID 和 nonce，校验 boot ID 的格式并将它返回。生命周期管理器在首次成功握手时持久化 boot ID，之后每次接管都与该值比对。探针使用的示例 initramfs 与此镜像分别构建。

| 输入或事件 | 可观察结果 |
| --- | --- |
| 合法 sandbox ID 与 nonce | 返回 `IDENTITY 1 <sandbox-id> <nonce> <boot-id>`。 |
| 版本、sandbox ID 或 nonce 不匹配，boot ID 格式无效，或应答超长、被截断 | 宿主拒绝把这条连接认作该 sandbox 的 guest。 |
| 同一 guest 上多次查询 | boot ID 保持一致；宿主据此识别 guest 是否在两次调用之间重新引导。 |

## 创建、恢复与删除

1. `RunPodSandbox` 先解析 handler、策略和所请求的 namespace、资源、网络配置。以原子方式登记 Pending attempt，再分配该 attempt 的资源。拒绝无法满足的请求，禁止回退到宿主容器执行。
2. 建立网络与 VM，等待 guest agent 握手，确认能够创建工作负载、Pod 所需的网络已经可用，再持久化 READY 并返回 ID。guest 启动失败或 CRI 取消时按已完成步骤反向回收。
3. `CreateContainer` 在同一 VM 内准备 OCI rootfs 和配置。`StartContainer` 才启动容器进程。`ExecSync`、streaming、状态和日志操作均定位该 guest 中的目标容器。容器停止后保存真实退出码、时间和日志。
4. daemon 重启时，用持久化的 sandbox ID、受管理的私有 socket、进程身份及 guest 标识重新确认所有权。仅凭 PID 可能命中已被复用的进程。身份不匹配或 guest 失联时报告 NOTREADY，并保留能够安全回收的记录。
5. `StopPodSandbox` 终止工作负载并释放网络；`RemovePodSandbox` 还要移除剩余容器及 VM 存储。以 sandbox 为粒度串行化状态变更。清理途中失败时保留重试信息，资源释放完毕后才删除持久化记录。

### 首版使用系统级 systemd 托管 VMM

启用实验性 VM 生命周期需要可使用 KVM 的 Linux 节点、运行中的系统级 systemd、可用的 `systemd-run` 和 `systemctl`，以及有权创建和停止临时服务的 nautilusd。普通 OCI 和 WASI 节点可沿用原配置。

每个 sandbox attempt 使用独立的临时服务。systemd 在 nautilusd 退出后继续持有 VMM 的进程组。Nautilus 在 `runtime.state_dir/vms` 记录 sandbox ID、启动 nonce、systemd InvocationID、私有目录及 socket 的文件身份和 guest boot ID。启动恢复时，匹配已登记的 VM 后重新连接，继续回收中断的创建或删除。遇到身份冲突时保留记录，在 CRI 中报告 NOTREADY，避免停止其他工作负载。

Cloud Hypervisor v55 在 HTTP 服务期间持有 `api.sock.lock` 的整文件 OFD 写锁。为每次新启动，Nautilus 在私有目录预先登记该锁的设备号、inode 和 nonce。VMM 被强制终止后，清理流程取得同类写锁并核对身份，再释放文件。升级时，运行中旧 VMM 的 systemd 身份、MainPID 打开的 inode 和持有的写锁可以补全记录。如果旧 VMM 已经退出，Nautilus 会将符合私有文件条件的未登记锁原子移至 `runtime.state_dir/vms/.api-lock-recovery/`，原 inode 和可追溯名称均保留。文件身份冲突或尚有进程持锁时，清理保留沙箱记录供重试。[Cloud Hypervisor v55 锁的实现](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v55.0/vmm/src/locked_unix_listener.rs#L41-L80)

实验入口为 `PodManager::start_pod`，镜像路径配置见 [nautilus.toml](../nautilus.toml) 的 `[runtime.sandboxes.vm]`。可通过 daemon 的 `PodSandboxStatus(verbose=true)` 读取 `info["nautilus.vm"]` 中的 `guestReachable` 和 `bootId`。成功验证时 `guestReachable` 为 true，确认缺失时为 false。状态查询发生错误时该字段为 null，`error` 给出原因。仅具备身份握手的开发用 Pod 保持 NOTREADY，启用 VM 工作负载时，通过完整网络和工作负载握手的 Pod 报告 READY，`StopPodSandbox` 和 `RemovePodSandbox` 可重试。开发机可用以下命令运行真实 VM 验证：

```sh
NAUTILUS_VM_VMM=/opt/cloud-hypervisor/bin/cloud-hypervisor \
NAUTILUS_VM_KERNEL=/opt/nautilus/guest/vmlinuz \
NAUTILUS_VM_INITRAMFS=/opt/nautilus/guest/agent.cpio.gz \
cargo test --locked --test vm_pod_lifecycle_test two_pods_survive_owner_restart -- --ignored --nocapture
```

在 Linux 5.15.0-191 + Cloud Hypervisor v54.1 和 Linux 6.12.118 + v55.0 两种宿主上，已用提交 `af327ce` 跑通上述生命周期测试。通过 PodManager 创建两台 VM，再从真实 `nautilusd` 的 CRI socket 核对两个 boot ID。强制终止和重启 daemon 后，两个 ID 均保持一致。并发停止、删除第一个 Pod 后，第二台 VM 仍可访问；独立关闭第二个 VMM 后，CRI 观察到退出，重复删除回收了私有目录和端点。运行时与镜像服务的回归结果见 [CRI 验证记录](CRITEST_RESULTS.md)。

### 每个 Pod 的 virtio-fs 导出

在 `[runtime.sandboxes.vm]` 设置 Rust `virtiofsd` 的绝对路径后，宿主先以独立的 systemd 临时服务创建 vhost-user socket，再以共享内存和标签 `podfs` 启动 VMM。每个 Pod 都有独立的导出根 `runtime.state_dir/vms/<sandbox-id>/podfs`。两个服务的 socket、PID 文件、启动令牌、所有权记录及镜像缓存均位于导出之外。未设置该选项时仍支持原先的 guest 身份验证。

宿主在同一文件系统的私有目录准备目录型 OCI bundle，包括 `config.json` 和 `rootfs/`。调用 `PodVmStore::publish_bundle` 后，guest 可以在挂载点下的 `containers/<container-uuid>/` 访问它。宿主固定目录文件描述符、核对目录身份并以不可覆盖的原子重命名发布；`remove_bundle` 可以重复调用。数据目录归 Pod 所有，宿主创建后不解析 guest 可写目录内的链接来寻址宿主资源。

受信任的节点 CRI 客户端通过每次 `CreateContainer.mounts` 指定宿主上的普通文件或目录。节点按 CRI 约定解析源符号链接，用文件描述符固定该次请求的 inode，并为每个容器挂载分配独立 UUID。路径被重命名或替换后，已有卷仍绑定原对象。guest 只收到 `volumes/<uuid>`。容器可按指定的只读或读写权限访问卷。宿主上的文件内容变化、目录内新增文件和 kubelet AtomicWriter 的 `..data` 切换可在 guest 中重新打开路径后观察。应用若依赖本机 inotify 事件，当前实现不保证跨 VM 通知。Kata 对需要通知的 Kubernetes 卷使用专门的 watchable bind。[上游 CRI Mount](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto#L335-L382) · [Kata 卷监视](https://github.com/kata-containers/kata-containers/blob/4.4.0/src/runtime/virtcontainers/container.go#L519-L548)

节点保留 virtiofsd 默认的挂载命名空间和 `pivot_root` 沙箱。启动 VMM 前，节点将该服务中的 `volumes/` 目录设为只读。随后通过 `open_tree` 固定请求时已存在的挂载树，在未发布的树上设置 `private`、`nosuid,nodev,noexec` 和读写属性，再用 `move_mount` 一次性接入该服务。节点本身的导出目录只包含空占位。每个卷的 Pod、容器、挂载 ID 和文件身份写入受保护的 owner 记录。删除容器会核对并卸载其卷；服务退出后内核释放整个私有挂载树。宿主源文件和目录由 CRI 调用方管理。旧版运行中的 Pod 可继续原有工作负载，重新创建 Pod 后即可申请宿主卷。[virtiofsd 沙箱](https://gitlab.com/virtio-fs/virtiofsd/-/blob/v1.13.2/src/sandbox.rs) · [Kata 在发布前设置只读](https://github.com/kata-containers/kata-containers/blob/4.4.0/src/runtime/virtcontainers/fs_share_linux.go#L428-L468) · [Firecracker 的挂载命名空间与 jail](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/docs/jailer.md)

请求宿主到容器或双向传播、CRI 递归只读、UID/GID 映射、SELinux 重标记、镜像卷或其他未实现的挂载选项会得到明确错误。节点需要 Linux 的 `mount_setattr`（5.12 起）以及加入该服务挂载命名空间的权限。

Firecracker v1.18.0 的 drives 接口对整个镜像文件或块设备设置只读属性。Nautilus 的 CRI 卷需要按单个文件和目录请求交付，使用 virtio-fs 保留这一级边界。[Firecracker 块设备](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/docs/block.md)

恢复和状态查询核对两个服务的 InvocationID、端点和导出目录身份。Stop/Remove 先停止 VMM，再停止 virtiofsd 并回收数据。身份校验失败时保留记录供重试。系统级 systemd 和 KVM 节点可用以下命令运行双 guest 验证：

```sh
export NAUTILUS_VM_VMM=/opt/cloud-hypervisor/bin/cloud-hypervisor
export NAUTILUS_VM_KERNEL="/boot/vmlinuz-$(uname -r)"
export NAUTILUS_VM_VIRTIOFSD=/usr/libexec/virtiofsd
export NAUTILUS_VM_INITRAMFS=/tmp/nautilus-share-test.cpio.gz
tests/fixtures/vm_share/build.sh "$NAUTILUS_VM_INITRAMFS"
cargo test --locked --test vm_pod_lifecycle_test -- --ignored --nocapture
```

2026-11-24 在 x86_64 Linux 5.15.0-191、Cloud Hypervisor v54.1、virtiofsd 1.13.2 上运行了该测试。两个 guest 同时使用 `podfs`，各自读到本 Pod 的随机标记并回写自己的 boot ID，另一个 Pod 的目录返回 ENOENT。通过 CRI 验证了 nautilusd 被 SIGKILL 后的接管、单个 virtiofsd 意外退出、另一个 VMM 退出和重复 Stop/Remove。缺失 virtiofsd、virtiofsd 启动即退出、VMM 启动即退出均完成回滚。另起未配置 virtiofsd 的两个 Pod 也完成了原身份测试。

### Pod 的初始资源

`linux.resources` 中的 CPU quota 和 period 决定 guest 的 vCPU 数量，向上取整。VMM 服务的 `cpu.max` 同时计入 `linux.overhead` 的带宽。CPU shares 映射为 cgroup v2 的 `cpu.weight`。内存限制向下对齐到 2 MiB 作为 guest RAM，VMM 服务的 `memory.max` 为请求字节数加上 Pod 开销。未指定 CPU 和内存限制时，guest 默认使用 1 个 vCPU、384 MiB RAM，服务继承所在 slice 的限制。目前接受 systemd slice 名或对应层级路径，cgroupfs 布局的父路径会在分配前返回错误。`RuntimeConfig` 返回与原生 OCI 后端一致的 cgroup 驱动。VM handler 使用 systemd 驱动运行。资源热更新返回 CRI `Unimplemented`。

使用 `bash guest/pack-initramfs.sh tests/fixtures/vm_resources_init.c /tmp/resources.cpio.gz` 构建资源测试 guest。将 `NAUTILUS_VM_RESOURCE_TEST_INITRAMFS` 指向该镜像后执行 `cargo test --locked --test vm_resources_test -- --ignored`，同时设置 VMM 和内核路径。guest 会核对在线 CPU 和可用物理内存，宿主测试会检查 VMM 配置及所在 cgroup。Linux 5.15.0-191、systemd 249、Cloud Hypervisor v54.1 上，1.5 CPU、448 MiB 内存加 0.1 CPU、64 MiB 开销，实测得到 2 个 vCPU、448 MiB guest RAM、`cpu.max=160000 100000` 和 `memory.max=536870912`。


### Pod VM 网络

`PodNet::reserve_vm` 为每个 sandbox attempt 持久化 CNI 配置、插件目录和网络命名空间的设备号及 inode。该 namespace 由 bind mount 持有。`attach_vm` 执行 CNI ADD，从实际 veth 读取 MAC、MTU 和 IPv4 路由，在同一 namespace 内创建 `nttap0`，用 tc 的 ingress redirect 将 veth 与 TAP 双向连接。VMM 服务通过 `NetworkNamespacePath` 进入该 namespace，以 `--net tap=nttap0,mac=<CNI MAC>` 打开 TAP。IP 由 guest 持有，宿主 veth 上不保留该地址。

当前支持显式配置的 CNI bridge、host-local IPAM、可选前置 loopback、单个 IPv4 地址及普通直连/网关路由。guest resolver 优先使用 CRI 的 DNS 配置，其次为 CNI result 中的 DNS，缺省时使用 `[network.dns]`。guest 构建需静态 BusyBox（可用 `NAUTILUS_GUEST_BUSYBOX` 指定）及匹配内核的 virtio_net 驱动，节点需要 iproute2 的 ip、tc 和 util-linux 的 nsenter。guest PID 1 应用地址、路由、MTU、loopback 和 resolver 后开始受理身份和网络握手。宿主调用 `VmNetwork::verify_guest`，核对本次 boot ID 和完整的已应用配置，再发布 READY。拒绝的配置包括多地址、IPv6 以及带高级属性的路由。

停止 VMM 后按 sandbox ID 调用 `teardown_vm`。启动失败、重启恢复或重复删除也走同一路径，即使 Pod 状态还未记录 namespace 路径。网络日志保留原始配置以供 CNI DEL 重试。绑定的 namespace 身份不符时保留记录，防止误拆其他 Pod。两台 guest 的网络验收在独立的 Linux mount 和 network namespace 中运行，环境变量 `NAUTILUS_CNI_BIN_DIR`、`NAUTILUS_VM_VMM`、`NAUTILUS_VM_KERNEL`、`NAUTILUS_VM_INITRAMFS` 分别指定 CNI 插件和 VM 资产。测试使用 dnsmasq、cpio、gzip，命令如下：

```sh
sudo --preserve-env=NAUTILUS_CNI_BIN_DIR,NAUTILUS_VM_VMM,NAUTILUS_VM_KERNEL,NAUTILUS_VM_INITRAMFS \
  unshare -mn --propagation private cargo test --locked --test vm_network_test -- --include-ignored --nocapture
```

### 实验性 CRI Pod VM

配置 `[runtime.sandboxes] vm_workloads_enabled = true` 以及 `[runtime.sandboxes.vm]` 的 VMM、kernel、工作负载 initramfs、virtiofsd 四个路径。默认关闭。显式发送 `RunPodSandbox(runtime_handler="vm")` 时，使用 bridge + host-local CNI、Pod 网络/IPC 和每容器私有 PID（`namespace_options.pid=CONTAINER`）。自定义 sysctl、hostPort、节点 namespace 和特定宿主安全配置会在分配资源前被拒绝。Pod 可指定 Linux hostname 和 DNS。

守护进程确认本次启动的身份、完整网络配置和工作负载 agent 的 4083 握手后才发布 READY，并返回该次 attempt 的 Pod IP。运行中 guest 失联会反映在状态及列表；独立的身份探针仍用于旧的开发用 VM。停止时先回收 guest 容器和 VMM，再以已记录的 CNI 配置删除网络。中断的启动或停止可在 daemon 重启时继续清理。

`tests/vm_pod_lifecycle_test.rs` 的 `cri_pod_network_survives_daemon_restart_and_releases_on_stop` 通过真实 CRI socket 验证两台 Pod VM、地址可达、daemon SIGKILL 后 IP 与 boot ID 保持、DEL 失败重试、VMM 意外退出、重复停止与删除。配置 `NAUTILUS_VM_IDENTITY_INITRAMFS` 可额外检查仅具有身份握手的 guest 启动失败后完整回收。通过同一测试的节点使用 Debian Linux 6.12.118，Cloud Hypervisor v54.1 和 v55.0 分别运行通过，guest 内核为 Ubuntu 5.15.0-191。

用 `guest/build-workload-initramfs.sh` 构建工作负载镜像。每个容器独立持有 OCI rootfs、PID namespace 和 guest runc 实例，通过认证的 vsock 连接进行生命周期和 ExecSync 操作。宿主按 guest 提供的日志偏移写入 kubelet 日志。日志文件系统需支持 `O_TMPFILE` 和用户扩展属性，宿主先持久化匿名 inode 的身份，再发布 kubelet 使用的文件名。[guest 协议](GUEST_OCI_PROTOCOL.md)

`tests/vm_cri_workloads_test.rs` 在独立网络命名空间启动真实 daemon，使用带摘要的本地 OCI 镜像测试两个 Pod、同 Pod 的两个容器、十次连续 exec、日志、超时、断连、daemon 重启和重复删除。运行时设置 `NAUTILUS_VM_VMM`、`NAUTILUS_VM_KERNEL`、`NAUTILUS_VM_INITRAMFS`、`NAUTILUS_VM_VIRTIOFSD`、`NAUTILUS_CNI_BIN_DIR` 和指向静态 BusyBox 的 `NAUTILUS_TEST_BUSYBOX`，再执行 `cargo test --locked --test vm_cri_workloads_test -- --ignored --nocapture`。在支持 OverlayFS 的节点同时设置 `NAUTILUS_VM_TEST_REQUIRE_SNAPSHOT=1`，可强制验证两个容器固定同一快照、upper 各自独立、写入与删除互相隔离、只读 rootfs 拒绝写入，以及重启后内容与清理结果。

在上述测试环境中设置 `NAUTILUS_TEST_CRICTL` 指向上游 crictl v1.37.0，即可同时运行真实客户端的 SPDY 验收。测试先读取一行 stdout，再写入 stdin，检查单独的 stderr 和退出码 35。它还验证终端尺寸变化、两次 attach 连到同一 init PID 1、断连后取消 exec、guest 回环端口的成功连接与拒绝，以及一次性 URL 的重用拒绝。

Kata 4.4.0 将执行、等待进程、读写 stdio 和调整终端尺寸作为 guest agent 操作。Firecracker v1.18.0 的 vsock 为宿主应用和 guest 应用提供双向字节通道。Nautilus 保留 kubelet 的标准 remotecommand 和 port-forward 接口，在已核对启动身份的 vsock 连接内由 guest agent 执行这些操作，转发端口从该 guest 的网络命名空间拨号。[Kata agent 接口](https://github.com/kata-containers/kata-containers/blob/4.4.0/src/libs/protocols/protos/agent.proto#L32-L48) · [Firecracker vsock](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/docs/vsock.md) · [Kubernetes remotecommand v4](https://github.com/kubernetes/client-go/blob/v0.37.2/tools/remotecommand/v4.go)

## 节点探针

本地执行 `nautilus vm probe` 会启动一台使用 1 个 vCPU、384 MiB 内存的临时 VM，经 guest 端口 4081 交换 `PING 1` 和 `PONG 1`，然后回收子进程和 socket。它可以在 nautilusd 启动之前运行。测试 guest 仅应答就绪请求，无网络、持久磁盘或执行任意命令的接口。

在 x86_64 Linux 验证机上准备已校验的 Cloud Hypervisor 可执行文件、可读的 guest 内核，以及与该内核版本匹配的模块。示例脚本使用静态 C 工具链、kmod、GNU cpio 和 gzip；压缩模块需要对应的解压工具。运行探针的账号需要能够读写 `/dev/kvm`。
若系统为 `TMPDIR` 设置了很深的目录，可用 `TMPDIR=/tmp` 运行探针，让生成的 Unix socket 路径满足 Linux 的长度限制。

```sh
./examples/vm-probe/build-initramfs.sh /tmp/nautilus-probe.cpio.gz "$(uname -r)"
nautilus vm probe --vmm /opt/cloud-hypervisor/bin/cloud-hypervisor \
  --kernel "/boot/vmlinuz-$(uname -r)" --initramfs /tmp/nautilus-probe.cpio.gz
```

主线使用 Cloud Hypervisor **v55.0**，其最低宿主 Linux 版本为 6.8，x86 要求 KVM 暴露 XSAVE2。Linux 5.15 机器使用 **v54.1** 作兼容性验证。运行探针时可显式传入另一个版本的 VMM 与 guest 镜像。后续发布记录分别列出宿主内核、VMM、guest 镜像和实际覆盖的 CRI 路径。[v55.0 宿主条件](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v55.0/README.md#host-os) · [v54.1 宿主条件](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v54.1/README.md#host-os)

2026-11-24 用完整的 Linux x86_64 CLI 和本仓库构造的 initramfs 做了两次 KVM 验证。原生宿主 Linux 5.15.0-191 + VMM v54.1 返回 `PONG 1`；同机启动的临时 Debian Linux 6.12.118 嵌套 KVM 宿主 + VMM v55.0 也返回 `PONG 1`。两次探测的目标 guest 均使用 Linux 5.15.0-191。无效 VMM 可用 `--vmm /bin/false` 验证：命令应快速报告 VMM 已退出，返回非零状态。错误应答与未应答的回归测试可用 `cargo test --locked --lib vm::tests` 运行。

### Apple Silicon PodSandbox 探针

macOS 的 M0 探针以 Apple Containerization `0.57.0` 的 `LinuxPod` 管理组内的 OCI 容器，直接用 Virtualization.framework 启动 guest。宿主引导和生命周期参考 Lima 的 VZ 后端、Colima 的 Lima 配置和 Kiri 的宿主与 guest 分工。PodSandbox 的 VM 归属参考 Kata Containers 4.4.0 的 Kubernetes Pod 模型。Firecracker v1.18.0 以 Linux KVM 和宿主 UNIX socket 提供虚拟化与 vsock，Mac 探针使用 Apple 的 VZ 设备接口。[探针与复现命令](../platform/macos/README.md) · [Kata 架构](https://github.com/kata-containers/kata-containers/blob/4.4.0/docs/design/architecture/README.md) · [Firecracker vsock](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/docs/vsock.md) · [Apple Virtualization](https://developer.apple.com/documentation/virtualization)

探针核对同组两个不同 OCI 镜像的独立可写 rootfs、同一 guest boot ID、组内 loopback、进程退出码、seccomp 和 VM 停止状态。对应的 Linux CRI 验收是通过 `RunPodSandbox` 创建 sandbox，再在其中分别用 `CreateContainer` 和 `StartContainer` 启动两个镜像，核对容器归属、执行结果和私有文件，最后通过 `StopPodSandbox` 和 `RemovePodSandbox` 验证回收。两端的跨组用例还需同时运行两个 sandbox，检查不同 boot ID，并验证停止其中一组时另一组仍能执行。M0 在 Mac 节点直接运行探针，后续里程碑通过 [Linux CRI 工作负载测试](../tests/vm_cri_workloads_test.rs)复验两端语义。

M1 的本地进程持有 VZ 对象。每次创建生成独立 sandbox ID 和工作目录，容器启动前准备各自的 OCI 根盘。单容器入口创建隐式 sandbox，显式清单在同一个 sandbox 中声明多个成员。组内提供 guest loopback 和共用的 tmpfs，每个容器有独立 PID namespace。采用 Kata 4.4.0 的 sandbox 归属方式，停止某个 attempt 不影响其他 attempt。Firecracker v1.18.0 的 jailer 为每个实例建沙箱目录，Mac 采用本地进程持有 VZ 和独立的根盘目录。CRI 对照用例是在两个 Ready 的 sandbox 中同时执行 `ExecSync`，调用 `StopPodSandbox` 停止第一个后再次执行第二个的命令，最后重复 `RemovePodSandbox`。[M1 复现](../platform/macos/README.md#run-workloads) · [Kata Pod 模型](https://github.com/kata-containers/kata-containers/blob/4.4.0/docs/design/architecture_4.0/architecture.md) · [Firecracker jailer](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/docs/jailer.md)

M2 的 VZ sandbox 默认没有 guest 网卡或宿主目录挂载。每个 sandbox 独立分配资源和 guest tmpfs，宿主显式提供的单个输入文件通过只读 virtiofs 副本传入，产物通过前台容器的有界 stdout 导出。Kata 4.4.0 在发布卷前设置只读标记，Firecracker v1.18.0 由宿主 jailer 约束 VMM 的文件与命名空间。Mac 用 VZ 设备配置限定 guest 可见的宿主资源，用进程生命周期与工作目录锁确定回收归属。取消命令时停止专用容器的私有 PID namespace。对应的 CRI 验收用例为同一 Pod 内让两个容器读取授权的只读文件，确认写入失败，再通过 `StopContainer` 停止其中一个容器，在另一容器中执行 `ExecSync` 核实服务可用。另建 sandbox 验证文件不可见。[M2 复现](../platform/macos/README.md#workload-policy) · [Kata 卷只读设置](https://github.com/kata-containers/kata-containers/blob/4.4.0/src/runtime/virtcontainers/fs_share_linux.go#L428-L468) · [Firecracker jailer](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/docs/jailer.md)

M3 用同一份 Redis 与 BusyBox 清单对照 macOS VZ 和 Linux CRI。两端验证只读输入、同组 loopback、前台退出码 `23`、产物字节和回收。验收同时记录两端的超时接口、网络和 seccomp 行为。[M3 对照与复现](PODSANDBOX_PARITY.md)

## Guest 工作负载统计

Kata Containers 4.4.0 的 agent 在 guest 中按容器 ID 查找 cgroup manager 并读取统计。Firecracker v1.18.0 的 metrics 接口由 VMM 输出设备与控制面计数。Nautilus 选用 guest cgroup v2 作为 CRI 工作负载用量来源。宿主 systemd cgroup 继续用于 VMM 的限制和节点运维。[Kata agent StatsContainer](https://github.com/kata-containers/kata-containers/blob/4.4.0/src/agent/src/rpc.rs#L1035-L1073) · [Firecracker metrics](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/docs/metrics.md) · [CRI CPU 和内存字段](https://github.com/kubernetes/cri-api/blob/v0.37.2/pkg/apis/runtime/v1/api.proto)

每个容器在 guest 的 `/sys/fs/cgroup/nautilus/<containerId>` 计量，包含该容器的 init 和 exec 后代。Pod 采样读取 `/sys/fs/cgroup/nautilus` 的层级计数，可累计已退出子 cgroup 的 CPU。guest init 和 agent 在该子树之外。一次经启动身份校验的 vsock 请求传回 Pod 及其运行中容器的采样，宿主按已记录的 Pod 归属补上 ID、标签和 attempt。[guest 协议](GUEST_OCI_PROTOCOL.md)

- CPU 累计值将 `cpu.stat usage_usec` 换算成纳秒。第二次采样与前一次至少相隔一秒时，agent 用单调时钟间隔计算平均 nanocores；更密集的请求沿用最近一次已测速率。
- 使用量为 `memory.current`。工作集为使用量减 `memory.stat inactive_file`，下限为零。RSS 取匿名内存 `anon` 加内核提供的 `swapcached`；随附的 initramfs 不启用 swap，旧内核无该字段时交换缓存为零。次缺页为 `pgfault - pgmajfault`，下限为零，主缺页取 `pgmajfault`。有限的 `memory.max` 减工作集得到可用内存。无限额时省略可用内存。基础统计请求保持轻量。独立的 `podMetrics` 使用同一 guest 身份校验，并为扩展采样设置单独预算。[Linux cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html)
- 验收通过真实 daemon 的四个 CRI 统计 RPC，令同一 Pod 的一个容器写入 16 MiB tmpfs 并计算摘要，另一个容器休眠。核对两者 CPU 增量、内存差异、Pod 汇总、列表的 ID 与标签组合筛选，以及另一个 guest 断连期间的可用性。[CRI 验收程序](../tests/vm_cri_workloads_test.rs)
- 扩展指标从同一采样读取 user/system CPU、CFS 周期和限流、内存缓存/匿名页/内核内存/映射文件及缺页计数。Pod 和容器各自报告本级 CFS 限流，Pod 计数不累加子 cgroup 的限流。验收令设定 CPU 配额的 VM 容器运行 CPU 密集任务，检查其限流计数增加、空闲邻居计数稳定、Pod 样本包含 tmpfs 对应缓存。两种 CRI 指标列表从同一来源采样。旧 agent 继续提供原有 CPU 和内存指标。[CRI 验收程序](../tests/vm_cri_workloads_test.rs)

- Pod 网络在 guest 内按接口采样一次，过滤回环和 cAdvisor 排除的接口。进程、FD、socket 和任务按各自 guest cgroup 归属，扫描分组与进程身份无法核实时省略对应值。
- 容器文件系统容量、inode 和用量来自独占的物化 rootfs。用量包含该副本的镜像块，容量反映承载文件系统。扫描按 inode 去重，限制目录遍历并排除挂载卷。每个 guest 同时只有一个文件系统采样 worker，超出等待预算时仍返回其他指标。Kata 4.4.0 同样在容器生命周期锁之外调用阻塞的统计读取。块 I/O 使用 guest 的 `io.stat`，当前 virtio-fs 根盘没有 guest 块设备时省略设备计数。Firecracker v1.18.0 的 VMM 设备指标描述整台 VM。 [Kata agent](https://github.com/kata-containers/kata-containers/blob/4.4.0/src/agent/src/rpc.rs#L1035-L1068) · [Firecracker metrics](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/docs/metrics.md) · [CRI 验证记录](CRITEST_RESULTS.md)

## 独立 CLI 工作负载和 kubelet

Kata Containers 4.4.0 经 containerd 向 Kubernetes 提供 VM RuntimeClass。其本地工具使用 containerd 的 runtime namespace 定位工作负载，Kubernetes 使用 `k8s.io`。Firecracker v1.18.0 是 VMM，历史 Firecracker-containerd 集成在归档提交 `6052d72` 中为独立实例配置自己的 socket 和状态目录。Nautilus 在同一个 CRI socket 上持久化独立 CLI Pod 的所有权。默认 CRI 清单、批量统计和生命周期事件仅反映 kubelet 工作负载，Nautilus CLI 通过自身请求标记保留完整清单。已有状态记录缺省属于普通 CRI。 [Kata 开发指南](https://github.com/kata-containers/kata-containers/blob/4.4.0/docs/Developer-Guide.md) · [Firecracker v1.18.0](https://github.com/firecracker-microvm/firecracker/releases/tag/v1.18.0) · [Firecracker-containerd 配置](https://github.com/firecracker-microvm/firecracker-containerd/blob/6052d721aea61a8cad3c347b0c07504ac95d869c/docs/getting-started.md)

验收在同一 daemon 上启动真实 kubelet 和独立 CLI。CLI 的 WASI 命令应保持可查询，退出后可读取其输出并执行 `--rm` 回收。通过标准 CRI 清单和事件订阅确认独立 CLI 的 Pod 与容器不可见，同时核对 Kubernetes 创建的 OCI 和 VM Pod 仍能通过原 CRI 路径查询及清理。

## 交付顺序

尚未交付的交互式启动、扩展指标、资源热更新与检查点按[后续优先级与设计](CRI_FOLLOW_UP.md)推进。该方案记录了与 Kata、Firecracker 的对照及每个后端的验收条件。

| 事项 | 依赖 | 合并前验收 |
| --- | --- | --- |
| [#2 节点与 guest 握手](https://github.com/nautilus-runtime/nautilus/issues/2) | 可独立实施 | 真实 KVM 上的 CLI 验证就绪协议；错误应答、超时和 VMM 提前退出均被识别并清理。 |
| [#3 每个 Pod 持有一台 VM](https://github.com/nautilus-runtime/nautilus/issues/3) | #2 | 两个 sandbox 各自拥有 VM；验证 daemon 重启、启动中断、并发停止/删除、重复调用与身份校验。 |
| [#4 guest 内 OCI 容器](https://github.com/nautilus-runtime/nautilus/issues/4) | #3 | 同一 VM 两个 OCI 容器分别创建、启动、执行与停止；十次调用保持工作区并正确返回退出码和日志。 |
| [#5 Pod 网络与资源](https://github.com/nautilus-runtime/nautilus/issues/5) | #3 | CNI、TAP、Pod IP、DNS 及资源约束可观察；网络失败、资源申请失败和删除重试释放全部资源。 |
| [#6 对 kubelet 开放 vm](https://github.com/nautilus-runtime/nautilus/issues/6) | #4、#5 | 启用后通过 CRI 发现 handler；真实 kubelet 验证 RuntimeClass、容器与流式操作、重启恢复和资源清理；复测原有 handler。 |

### 可并行开发的子任务

| 子任务 | 输入依赖 | 验收入口 |
| --- | --- | --- |
| [#11 每个 Pod 独立 virtiofsd](https://github.com/nautilus-runtime/nautilus/issues/11) | #3 | 两台 VM 的导出目录隔离，启动失败和 daemon 重启后可回收。 |
| [#12 guest OCI 管理](https://github.com/nautilus-runtime/nautilus/issues/12) | guest 身份协议 | 真实 guest 内管理两个容器，验证十次 exec、日志和退出状态。 |
| [#13 CNI、TAP 和 guest 网络](https://github.com/nautilus-runtime/nautilus/issues/13) | #3 | 从 guest 验证对端、DNS、Pod IP 和失败回收。 |
| [#14 Pod CPU 和内存](https://github.com/nautilus-runtime/nautilus/issues/14) | #3 | 对照 VMM 启动配置和宿主内核的 cgroup 限制。 |
| [#15 CRI 容器生命周期](https://github.com/nautilus-runtime/nautilus/issues/15) | #11、#12 | 经真实 daemon socket 运行 OCI 镜像，验证状态、工作区、日志和恢复。 |
| [#16 RuntimeClass 与 kubelet](https://github.com/nautilus-runtime/nautilus/issues/16) | #13、#14、#15 | 经真实 kubelet 验证调度、流式操作与清理，运行完整 critest。 |

#16 分为 [#21 宿主卷实时交付](https://github.com/nautilus-runtime/nautilus/issues/21)、[#23 Exec / Attach / PortForward](https://github.com/nautilus-runtime/nautilus/issues/23) 和 [#22 真实 kubelet 及 RuntimeClass 验收](https://github.com/nautilus-runtime/nautilus/issues/22)。前两项可独立开发，最后一项在真实集群核对两条路径。

每个 Pod 使用专属的 virtiofsd 导出容器 bundle。Kata 4.4.0 的 Rust runtime 将宿主传入的 rootfs 挂载后通过 virtio-fs 共享，containerd v2.4.2 的 OverlayFS snapshotter 为每个 active snapshot 分配独立的可写层并复用已提交的父层。Firecracker v1.18.0 的块设备接口使用宿主文件作为磁盘后端。Nautilus 沿用现有 virtio-fs bundle 路径，复用 PullImage 生成的不可变镜像树，避免为每个容器建立磁盘或重新解包镜像。[Kata 4.4.0 rootfs 共享](https://github.com/kata-containers/kata-containers/blob/4.4.0/src/runtime-rs/crates/resource/src/rootfs/share_fs_rootfs.rs) · [containerd v2.4.2 OverlayFS snapshotter](https://github.com/containerd/containerd/blob/v2.4.2/plugins/snapshots/overlay/overlay.go) · [Firecracker v1.18.0 块设备](https://github.com/firecracker-microvm/firecracker/blob/v1.18.0/docs/block.md)

每个 VM 容器在宿主私有目录持有独立的 `upper`、`work` 和快照引用。daemon 将合成视图接入该 Pod 的 virtiofsd 挂载命名空间，guest 仍从 `containers/<id>/rootfs` 启动 OCI 容器。挂载前记录 bundle 和挂载身份，删除时先回收 guest 容器，再卸载视图、释放引用。镜像清理会保留仍有引用的树。关闭 OverlayFS、存储不满足快照探测条件或沿用早期 Pod 的导出时，使用原有逐容器解包路径。扩展文件系统指标沿用 guest 的容量与设备标签，可写用量只计算当前容器的 upper，并受单次采样预算约束。[Issue #76](https://github.com/nautilus-runtime/nautilus/issues/76)

首版范围为 Linux CPU 工作负载。GPU 直通、跨主机迁移和 VM 快照需要独立的资源与一致性设计。Firecracker 和 Dragonball 保留为后续 VMM 候选。工作负载兼容性由 CRI 验收结果决定，节点探针与性能测量单独记录。
