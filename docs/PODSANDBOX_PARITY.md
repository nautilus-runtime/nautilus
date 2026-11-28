# M3 · macOS 与 Linux CRI PodSandbox 对照

同一份 [成功用例](../tests/podsandbox-parity/success.json)、[超时用例](../tests/podsandbox-parity/timeout.json)和[输入文件](../tests/podsandbox-parity/input.txt)分别走 macOS 本地 `pod` 命令和 Linux `vm` runtime handler。两端以相同 OCI index 固定 BusyBox 和 Redis，按 guest 架构解析镜像。Linux 验收直接调用 CRI v1 的 Runtime 和 Image 服务。[里程碑 #81](https://github.com/nautilus-runtime/nautilus/issues/81) · [CRI VM 契约](CRI_VM_CONTRACT.md)

## 验收环境

| 执行环境 | 宿主与虚拟化 | Guest 与入口 |
| --- | --- | --- |
| 本机 macOS | Apple M4，macOS 27.2.1，Darwin 27.2.0，Virtualization.framework build 308.2.7 | Linux 6.18.56，Apple Containerization 0.57.0，runc 1.5.3，已签名的 `nautilus-pod-probe pod` |
| pro6000 | x86_64，Ubuntu 22.04.5，宿主 Linux 5.15.0-204-generic，Cloud Hypervisor v54.1，virtiofsd 1.14.0，CNI plugins 1.10.0 | Linux 5.15.0-204-generic，Nautilus workload agent，runc 1.5.3，`crictl` v1.37.0 |

验收基线为 main `3ace00e`。Linux daemon 使用 #84 的 x86_64 CI 产物并核对 SHA-256，该构建与基线的 Rust、CRI proto 及 guest 源码一致。Linux guest initramfs 从该基线源码和匹配的内核模块构建。[Linux 构建产物](https://github.com/nautilus-runtime/nautilus/actions/runs/46145003460)

| 镜像 | OCI index SHA-256 | macOS guest `linux/arm64` manifest SHA-256 | Linux guest `linux/amd64` manifest SHA-256 |
| --- | --- | --- | --- |
| BusyBox | `a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec` | `d82c2ab94640ded77cf76514ce6a84870761105058a4a9e51b05a8a79be97a6c` | `66a6306db78bf2dbf3487f293aa8d6990d8e506fdffab9cc43fe422becf886e4` |
| Redis | `5af80107c1eeecb484a2269b80a26d12bfa6521787da4663bd9f19f60a163483` | `95f35d033e2a07b986450bd9c6191fd5ca04adf3a9ff1ba815f087059ee120c8` | `bf33e63600ae8240c9f8b553c8fae427550dae17042bc24b06bbe06d3057c9bc` |

## 2026-11-28 验收结果

两端通过正常和超时场景。正常场景中，BusyBox 读取只读输入，通过 `127.0.0.1` 将数据写入 Redis 并读回。应用和数据库在同一 guest 内，boot ID 相同，PID namespace 各自独立。应用使用清单的环境变量和工作目录，返回 `23`。两端逐字节核对正常产物及超时前输出。

| 行为 | macOS VZ | pro6000 Linux CRI |
| --- | --- | --- |
| 输入和工作区 | 单文件只读副本挂到 `/input/file`，组内共享 guest tmpfs | 单文件目录只读挂载到 `/input`，组内共享每个 sandbox 专用的宿主目录。两个并行 sandbox 的标记文件不可互读 |
| 显式输出 | CLI 将选定的 stdout 导出到新建的 `0600` 文件 | 调用方通过 `crictl logs --stream=stdout` 读取选定容器的 stdout |
| 8 秒超时 | CLI 返回 `124`，导出 `timeout-ready` 并回收整个 sandbox | 外部 supervisor 使用 `StopContainer`，CRI 报告 `137`。已脱离会话的子进程心跳停止，两个并行 sandbox 的 Redis 仍可用，随后由调用方回收 |
| guest 网络 | 只有 `lo`，无默认路由 | CNI 提供 `lo` 和 `eth0`。隔离验收配置下无 IPv4 默认路由，guest 对 TEST-NET-2 地址 `198.51.100.1` 的路由查询和连接均失败 |
| 容器安全 | `NoNewPrivs=1`，`Seccomp=2`，一个过滤器 | `NoNewPrivs=1`，`Seccomp=0`，零过滤器。显式申请 `RuntimeDefault` 的容器在 `CreateContainer` 被拒绝 |
| 回收 | 两次调用结束后私有 session 状态均已释放 | CRI Pod 和容器、VM 状态、网络命名空间及 IPAM 租约均已清空 |

正常产物 SHA-256 为 `26df097f3a92b719e2bf4faaf6043d987de5a29612bbb744b6432723b3d07e90`，超时产物为 `1e761275cad46139e5f2e02fe2cddf88b55a5639a06c1b0be7506383222e48bb`。工作负载和产物已对照通过。Linux VM 的 seccomp 仍是策略差异，要求 `RuntimeDefault` 的工作负载会在创建阶段返回错误。[Linux VM profile 校验](../src/sandbox/vm_backend.rs) · [CRI 回归基线](CRITEST_RESULTS.md)

## 复验

在仓库根目录运行。macOS 使用带 `com.apple.security.virtualization` entitlement 的探针，以及已校验的 kernel 和 guest rootfs。[构建与签名](../platform/macos/README.md#run-the-probe)

```sh
rtk proxy sh tests/podsandbox-parity/run-macos.sh "$SIGNED_PROBE" "$MAC_ASSETS" "$MAC_WORK_PARENT"
```

从能访问 Docker Hub 的机器准备离线镜像缓存。脚本校验 OCI index 的 SHA-256，并核对平台 manifest、配置和每层内容的大小及 SHA-256。复制缓存和本目录文件到独立 Linux 验收节点。测试资产目录应包含 `cloud-hypervisor`、`virtiofsd`、`crictl`、`initramfs.cpio.gz` 和 `cni/{bridge,host-local,loopback}`。guest initramfs 使用当前仓库和与 guest kernel 匹配的模块构建。[guest 构建说明](GUEST_OCI_PROTOCOL.md)

```sh
rtk proxy python3 tests/podsandbox-parity/prepare-linux-cache.py \
  tests/podsandbox-parity/success.json "$IMAGE_CACHE" amd64
# 在 Linux 验收节点运行，REPORT_DIR 为已有目录。
rtk proxy sudo bash tests/podsandbox-parity/run-isolated-linux.sh \
  "$NAUTILUSD" "$LINUX_ASSETS" "$GUEST_KERNEL" "$IMAGE_CACHE" "$REPORT_DIR"
```

Linux runner 以独立网络命名空间启动 daemon，使用独立的状态目录、CRI socket、CNI bridge 和 IPAM。bridge 启用端口隔离，无默认路由和 NAT。镜像流量在 guest 启动前完成。runner 将验收与 daemon 日志写入 `REPORT_DIR`，验收通过后回收 daemon 和临时目录。CNI 端口隔离配置可限制同桥 Pod 之间的连接，本用例从 guest 实测的是外网地址不可路由，Redis 按工作负载要求仅监听 guest loopback。[CNI bridge 源码](https://github.com/containernetworking/plugins/blob/v1.10.0/plugins/main/bridge/bridge.go)
