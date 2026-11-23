# Nautilus CRI 兼容性测试方案

## 📋 测试概述

本文档描述了使用 critest 对 Nautilus CRI 容器运行时进行兼容性测试的完整方案。critest 是 Kubernetes 官方提供的 CRI 兼容性测试工具，用于验证容器运行时是否符合 CRI 规范。

## 🎯 测试目标

- 验证 Nautilus 与 Kubernetes CRI API 的完全兼容性
- 确保所有 CRI 接口正确实现
- 验证容器和 Pod 生命周期管理
- 测试镜像管理功能
- 验证网络和存储集成

## 🔧 测试环境要求

### 系统环境
- **操作系统**: Debian 12 (bookworm)
- **内核版本**: 6.1.0
- **架构**: x86_64
- **容器化**: 支持 Docker 环境测试

### 依赖组件
- **Go**: >= 1.19 (用于编译 critest)
- **runc**: >= 1.1.0
- **conmon-rs**: >= 0.6.0
- **CNI plugins**: >= 1.0.0
- **critest**: Kubernetes CRI 测试工具

## 📦 测试环境准备

### 1. 安装系统依赖
```bash
# 更新系统包
sudo apt update && sudo apt upgrade -y

# 安装基础依赖
sudo apt install -y \
    build-essential \
    pkg-config \
    libseccomp-dev \
    libsystemd-dev \
    git \
    curl \
    wget \
    jq

# 安装 Go
wget https://go.dev/dl/go1.21.5.linux-amd64.tar.gz
sudo tar -C /usr/local -xzf go1.21.5.linux-amd64.tar.gz
echo 'export PATH=$PATH:/usr/local/go/bin' >> ~/.bashrc
source ~/.bashrc
```

### 2. 安装容器运行时依赖
```bash
# 安装 runc
wget https://github.com/opencontainers/runc/releases/download/v1.1.10/runc.amd64
sudo mv runc.amd64 /usr/bin/runc
sudo chmod +x /usr/bin/runc

# 安装 CNI plugins
wget https://github.com/containernetworking/plugins/releases/download/v1.3.0/cni-plugins-linux-amd64-v1.3.0.tgz
sudo mkdir -p /opt/cni/bin
sudo tar -C /opt/cni/bin -xzf cni-plugins-linux-amd64-v1.3.0.tgz

# 创建 CNI 配置目录
sudo mkdir -p /etc/cni/net.d
```

### 3. 编译和安装 critest
```bash
# 克隆 cri-tools 仓库
git clone https://github.com/kubernetes-sigs/cri-tools.git
cd cri-tools

# 编译 critest
make critest

# 安装到系统路径
sudo cp build/bin/linux/amd64/critest /usr/local/bin/
sudo chmod +x /usr/local/bin/critest

# 验证安装
critest --version
```

## 🚀 测试执行方案

### 测试阶段划分

#### 阶段 1: 基础功能测试
- CRI 服务连接测试
- 版本信息查询
- 运行时状态检查

#### 阶段 2: 镜像管理测试
- 镜像拉取功能
- 镜像列表查询
- 镜像删除功能
- 镜像状态查询

#### 阶段 3: Pod 生命周期测试
- Pod 创建和启动
- Pod 状态查询
- Pod 列表功能
- Pod 停止和删除

#### 阶段 4: 容器生命周期测试
- 容器创建和启动
- 容器状态查询
- 容器列表功能
- 容器停止和删除
- 容器日志功能

#### 阶段 5: 高级功能测试
- 网络配置测试
- 存储挂载测试
- 资源限制测试
- 安全上下文测试

### 测试配置文件

#### CNI 网络配置
```json
{
  "cniVersion": "1.0.0",
  "name": "nautilus-bridge",
  "type": "bridge",
  "bridge": "nautilus0",
  "isGateway": true,
  "ipMasq": true,
  "ipam": {
    "type": "host-local",
    "subnet": "172.20.0.0/16",
    "gateway": "172.20.0.1"
  }
}
```

#### Nautilus 测试配置
```toml
[runtime]
state_dir = "/tmp/nautilus-test/state"
log_level = "debug"
max_containers = 100
default_runtime = "runc"
worker_threads = 2

[runtime.runc]
path = "/usr/bin/runc"
root = "/tmp/nautilus-test/runc"
systemd_cgroup = false

[runtime.conmon]
path = "/usr/bin/conmon-rs"
log_size_max = "10MB"
log_tag = "nautilus-test"

[storage]
root_dir = "/tmp/nautilus-test/storage"
image_dir = "/tmp/nautilus-test/images"
container_dir = "/tmp/nautilus-test/containers"
tmp_dir = "/tmp/nautilus-test/tmp"
max_image_size = "1GB"
gc_threshold = 0.9

[network]
cni_config_dir = "/etc/cni/net.d"
cni_bin_dir = "/opt/cni/bin"
default_network = "nautilus-bridge"
bridge_name = "nautilus0"
subnet = "172.20.0.0/16"
gateway = "172.20.0.1"

[cri]
listen_address = "unix:///tmp/nautilus-test/nautilus.sock"
max_concurrent_streams = 100
request_timeout = "30s"
stream_idle_timeout = "1h"

[monitoring]
enable_metrics = true
metrics_address = "127.0.0.1:9091"
log_directory = "/tmp/nautilus-test/logs"
max_log_size = "50MB"
log_rotation = 3

[security]
enable_selinux = false
enable_apparmor = false
allow_privileged = false
```

## 📊 测试指标和验收标准

### 功能性指标
- **CRI API 覆盖率**: 100% (所有必需的 CRI 接口)
- **测试通过率**: >= 95%
- **关键功能**: Pod/容器生命周期管理必须 100% 通过

### 性能指标
- **容器启动时间**: < 2 秒 (P95)
- **镜像拉取时间**: < 30 秒 (小镜像)
- **API 响应时间**: < 500ms (P95)

### 稳定性指标
- **内存泄漏**: 无明显内存增长
- **资源清理**: 测试后无残留进程/文件
- **错误恢复**: 异常情况下能正确恢复

## 🔍 问题诊断和解决

### 常见问题类型

#### 1. 连接问题
- CRI socket 权限问题
- 网络配置错误
- 服务启动失败

#### 2. 运行时问题
- runc 配置错误
- cgroup 权限问题
- 命名空间创建失败

#### 3. 网络问题
- CNI 插件缺失
- 网络配置错误
- 防火墙规则冲突

#### 4. 存储问题
- 磁盘空间不足
- 权限配置错误
- 镜像格式不兼容

### 调试工具和方法

#### 日志分析
```bash
# 查看 Nautilus 日志
tail -f /tmp/nautilus-test/logs/nautilus.log

# 查看系统日志
journalctl -f -u nautilus

# 查看容器日志
runc list
runc events <container-id>
```

#### 网络诊断
```bash
# 检查网络接口
ip link show
ip addr show nautilus0

# 检查 CNI 配置
cat /etc/cni/net.d/*.conf

# 测试网络连通性
ping 172.20.0.1
```

#### 存储诊断
```bash
# 检查存储空间
df -h /tmp/nautilus-test

# 检查权限
ls -la /tmp/nautilus-test/

# 检查镜像存储
find /tmp/nautilus-test/images -type f
```

## 📈 测试报告模板

### 测试执行摘要
- **测试时间**: [开始时间] - [结束时间]
- **测试环境**: [环境描述]
- **测试版本**: [Nautilus 版本]
- **总测试数**: [数量]
- **通过测试**: [数量]
- **失败测试**: [数量]
- **跳过测试**: [数量]

### 详细测试结果
- **基础功能**: [通过/失败/跳过]
- **镜像管理**: [通过/失败/跳过]
- **Pod 生命周期**: [通过/失败/跳过]
- **容器生命周期**: [通过/失败/跳过]
- **高级功能**: [通过/失败/跳过]

### 性能指标
- **平均容器启动时间**: [时间]
- **平均 API 响应时间**: [时间]
- **内存使用峰值**: [内存量]
- **CPU 使用峰值**: [百分比]

### 问题和建议
- **发现的问题**: [问题列表]
- **修复建议**: [建议列表]
- **改进方向**: [改进建议]

## 🔄 持续集成建议

### CI/CD 集成
- 在每次代码提交后自动运行 critest
- 设置测试通过率阈值
- 自动生成测试报告
- 失败时自动通知开发团队

### 测试环境管理
- 使用容器化测试环境
- 自动清理测试数据
- 并行执行测试以提高效率
- 定期更新测试依赖

---

**注意**: 本测试方案需要根据实际环境和需求进行调整。建议在正式测试前先进行小规模验证。