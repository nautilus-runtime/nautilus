# 在 Kubernetes 节点启用 Pod VM

`runtimeClassName: nautilus-vm` 让 kubelet 使用 Nautilus 的 `vm` handler。一个 PodSandbox attempt 拥有一台 Cloud Hypervisor 虚机。同一 Pod 的 OCI 容器共享 guest 网络，可以分别执行命令和读取日志。

## 准备节点

- 使用 Linux、KVM、cgroup v2 和系统级 systemd。Cloud Hypervisor v55.0 的 x86 节点需要 Linux 6.8 或更新内核及 KVM XSAVE2。pro6000 的 Linux 5.15.0-191 物理宿主验证了 v54.1，社区版 v55.0 在其 Linux 6.12.118 嵌套 KVM 节点通过验收。[Cloud Hypervisor 平台要求](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v55.0/README.md#host-os)
- 准备 virtiofsd、IPv4 bridge 和 host-local CNI 插件，以及支持 `O_TMPFILE` 和用户扩展属性的 kubelet 日志文件系统。宿主内核、guest 内核和 guest 模块可以有不同版本。构建 initramfs 时传入与 **guest 内核** 对应的模块版本。[guest 构建说明](GUEST_OCI_PROTOCOL.md)
- 从本仓库构建工作负载 initramfs，并在节点配置实际的 VMM、guest kernel、initramfs、virtiofsd 和 CNI 路径。以下 TOML 列出需要设置的字段，可以直接修改现有配置中的同名字段。[配置文件](../nautilus.toml)

```toml
[runtime.runc]
systemd_cgroup = true

[runtime.sandboxes]
vm_workloads_enabled = true

[runtime.sandboxes.vm]
vmm = "/opt/cloud-hypervisor/bin/cloud-hypervisor"
kernel = "/opt/nautilus/guest/vmlinuz"
initramfs = "/opt/nautilus/guest/workload.cpio.gz"
virtiofsd = "/usr/libexec/virtiofsd"
```

让 kubelet 的 CRI 端点指向该节点的 nautilusd，例如 `unix:///run/nautilus/nautilus.sock`。按节点现有部署方式重新加载 daemon 配置。启动后，nautilusd 会创建并回收测试 VM，核对 guest 身份、网络和工作负载 agent。成功后，kubelet 会将 `vm` 写入 Node 的 `status.runtimeHandlers`。把下面的 `NODE` 设为实际节点名，确认输出包含独立一行 `vm` 后再标记它可调度 VM。

```sh
NODE=my-kvm-node
kubectl get node "$NODE" -o jsonpath='{range .status.runtimeHandlers[*]}{.name}{"\n"}{end}'
kubectl label node "$NODE" nautilus-vm=enabled --overwrite
```

## 创建 RuntimeClass 和测试 Pod

在具有该标签的节点上调度。示例镜像与 Kubernetes v1.37.2 的实测一致，Pod 内的 `/bin/sh` 会打印当前 guest 的 boot ID 并继续运行。

```sh
kubectl apply -f - <<'YAML'
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: nautilus-vm
handler: vm
scheduling:
  nodeSelector:
    nautilus-vm: enabled
---
apiVersion: v1
kind: Pod
metadata:
  name: nautilus-vm-smoke
spec:
  runtimeClassName: nautilus-vm
  restartPolicy: Never
  automountServiceAccountToken: false
  containers:
    - name: app
      image: registry.k8s.io/e2e-test-images/busybox:1.29-2
      command: ["/bin/sh", "-c", "cat /proc/sys/kernel/random/boot_id; sleep 3600"]
      resources:
        requests: {cpu: "1", memory: 512Mi}
        limits: {cpu: "1", memory: 512Mi}
YAML
kubectl wait --for=condition=Ready pod/nautilus-vm-smoke --timeout=180s
kubectl get pod nautilus-vm-smoke -o wide
kubectl logs nautilus-vm-smoke
kubectl exec nautilus-vm-smoke -- cat /proc/sys/kernel/random/boot_id
kubectl delete pod nautilus-vm-smoke --wait
```

日志和 exec 返回相同的 boot ID。另一个 VM Pod 有独立的 guest 和 Pod IP。示例删除后，可复用 RuntimeClass 和节点标签。

## 已验证的行为与范围

真实 kubelet 验收使用 Kubernetes v1.37.2 和 Cloud Hypervisor v55.0。两个 VM Pod 中共有三个容器，验证了跨 Pod HTTP、真实退出码、连接原 init 的 attach、guest 回环端口转发、CRI 日志，以及 daemon 重启后 boot ID、Pod IP 和容器身份保持。WASI 和默认 OCI 后端另行回归。[CRI 回归结果](CRITEST_RESULTS.md)

- 受信任的节点 CRI 调用方可以指定任意宿主普通文件或目录。每个 Pod 单独导出被请求的挂载，支持只读或读写。Kubernetes 的准入策略决定哪些 Pod 可以使用 hostPath。ConfigMap 通过 AtomicWriter 切换后，在 guest 中重新打开文件可读到新值。应用若依赖 inotify，需要单独验证通知语义。[卷交付](VM_RUNTIME.md)
- 首版支持私有 PID namespace、Pod 网络和 IPC，以及创建时的 Linux CPU / 内存约束。资源热更新和 guest CPU / 内存用量统计目前未开放。节点上的 VMM cgroup 可用来核实整台 VM 的资源约束。[资源契约](CRI_VM_CONTRACT.md)
- pro6000 上的验收覆盖 CPU 工作负载。GPU 直通、快照和迁移分别需要额外的 VMM 设备与生命周期支持。
