# Pod 级虚机运行时

设计跟踪于 [#1](https://github.com/nautilus-runtime/nautilus/issues/1)。目标是在 Kubernetes 中为工作负载选择 `RuntimeClass.handler: vm`，让一次 PodSandbox attempt 拥有一台 Linux VM。同一沙箱内的容器在该 guest 中运行，重复 exec 和工具调用沿用原容器的工作区。显式启用实验 VM 的节点支持通过 CRI 创建 Pod、运行 guest OCI 容器、执行 ExecSync 及恢复日志和网络。交互式 Exec、Attach 和 PortForward 连接到目标 guest。kubelet 的 RuntimeClass 发现和宿主卷接入跟踪于 #16。[CRI v1 契约与本地差异](CRI_VM_CONTRACT.md)

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

实验入口为 `PodManager::start_pod`，镜像路径配置见 [nautilus.toml](../nautilus.toml) 的 `[runtime.sandboxes.vm]`。可通过 daemon 的 `PodSandboxStatus(verbose=true)` 读取 `info["nautilus.vm"]` 中的 `guestReachable` 和 `bootId`。成功验证时 `guestReachable` 为 true，确认缺失时为 false。状态查询发生错误时该字段为 null，`error` 给出原因。已启动的实验 Pod 保持 NOTREADY，`StopPodSandbox` 和 `RemovePodSandbox` 可重试。开发机可用以下命令运行真实 VM 验证：

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

`linux.resources` 中的 CPU quota 和 period 决定 guest 的 vCPU 数量，向上取整。VMM 服务的 `cpu.max` 同时计入 `linux.overhead` 的带宽。CPU shares 映射为 cgroup v2 的 `cpu.weight`。内存限制向下对齐到 2 MiB 作为 guest RAM，VMM 服务的 `memory.max` 为请求字节数加上 Pod 开销。未指定 CPU 和内存限制时，guest 默认使用 1 个 vCPU、384 MiB RAM，服务继承所在 slice 的限制。目前接受 systemd slice 名或对应层级路径，cgroupfs 布局的父路径会在分配前返回错误。kubelet 的 cgroup 驱动发现与节点配置在 #16 验收。资源热更新返回 CRI `Unimplemented`。

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

`tests/vm_cri_workloads_test.rs` 在独立网络命名空间启动真实 daemon，使用带摘要的本地 OCI 镜像测试两个 Pod、同 Pod 的两个容器、十次连续 exec、日志、超时、断连、daemon 重启和重复删除。运行时设置 `NAUTILUS_VM_VMM`、`NAUTILUS_VM_KERNEL`、`NAUTILUS_VM_INITRAMFS`、`NAUTILUS_VM_VIRTIOFSD`、`NAUTILUS_CNI_BIN_DIR` 和指向静态 BusyBox 的 `NAUTILUS_TEST_BUSYBOX`，再执行 `cargo test --locked --test vm_cri_workloads_test -- --ignored --nocapture`。

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

## 交付顺序

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

首版使用每个 Pod 专属的 virtiofsd 导出容器 bundle。宿主复用 Nautilus 拉取的 OCI 镜像，为每个容器准备独立 rootfs。对 CRI 卷只开放已能兑现权限、读写和传播语义的类型。Kata 4.4.0 的 Cloud Hypervisor 配置也默认使用 virtio-fs，并为块设备 snapshotter 提供单独的接入路径。[Kata Go runtime 默认配置](https://github.com/kata-containers/kata-containers/blob/4.4.0/src/runtime/Makefile#L266-L267) · [Kata Rust runtime 默认配置](https://github.com/kata-containers/kata-containers/blob/4.4.0/src/runtime-rs/Makefile#L240) · [Kata 存储设计](https://github.com/kata-containers/kata-containers/blob/4.4.0/docs/design/architecture/storage.md)

首版范围为 Linux CPU 工作负载。GPU 直通、跨主机迁移和 VM 快照需要独立的资源与一致性设计。Firecracker 和 Dragonball 保留为后续 VMM 候选。工作负载兼容性由 CRI 验收结果决定，节点探针与性能测量单独记录。
