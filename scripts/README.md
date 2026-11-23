# Nautilus CRI 测试脚本使用指南

本目录包含了用于测试 Nautilus CRI 兼容性的脚本集合。所有脚本都已优化为在非 root 用户下运行，需要 sudo 权限时会自动提示。

## 脚本概览

### 1. nautilus_critest.sh (推荐)
**主要的统一测试脚本**，提供完整的测试功能和多种测试模式。

```bash
# 运行本地完整测试（推荐）
./nautilus_critest.sh local

# 运行快速容器测试
./nautilus_critest.sh quick

# 运行容器化测试
./nautilus_critest.sh container

# 仅安装依赖
./nautilus_critest.sh install-deps

# 清理环境
./nautilus_critest.sh clean

# 显示帮助
./nautilus_critest.sh help
```

### 2. critest_runner.sh
**本地测试脚本**，直接在物理环境中运行 CRI 测试。

```bash
# 运行本地测试
./critest_runner.sh
```

### 3. docker_critest.sh
**容器化测试脚本**，在 Docker 容器中运行测试。

```bash
# 构建测试镜像
./docker_critest.sh build

# 运行容器化测试
./docker_critest.sh run

# 运行本地容器测试
./docker_critest.sh local

# 清理环境
./docker_critest.sh clean
```

### 4. simple_critest.sh
**简化测试脚本**，主要用于容器内部环境。

```bash
# 运行基础测试
./simple_critest.sh basic

# 运行完整测试
./simple_critest.sh full
```

## 测试模式说明

### 本地测试 (local)
- **优点**: 功能最完整，性能最好，调试方便
- **缺点**: 需要安装较多依赖
- **适用场景**: 开发环境，详细测试

### 容器化测试 (container)
- **优点**: 环境隔离，依赖管理简单
- **缺点**: 需要构建镜像，启动较慢
- **适用场景**: CI/CD 环境，生产验证

### 快速测试 (quick)
- **优点**: 启动快速，无需预构建镜像
- **缺点**: 功能有限，仅基础测试
- **适用场景**: 快速验证，基础功能检查

## 系统要求

### 基础要求
- Linux 操作系统
- 非 root 用户（脚本会自动检查）
- sudo 权限（用于安装依赖和系统配置）

### 依赖组件
脚本会自动安装以下依赖：
- Go 1.21.5+
- Rust (最新稳定版)
- runc 1.1.10
- CNI 插件 v1.3.0
- critest (最新版)
- 系统工具: curl, wget, git, jq, bc, tar, make

### Docker 要求（容器化测试）
- Docker Engine
- 当前用户需要 Docker 权限或 sudo 权限

## 使用步骤

### 快速开始

1. **运行本地测试**（推荐新手）:
   ```bash
   cd scripts
   ./nautilus_critest.sh local
   ```

2. **运行快速验证**:
   ```bash
   ./nautilus_critest.sh quick
   ```

### 详细步骤

1. **检查环境**:
   ```bash
   # 确保在非 root 用户下
   whoami
   
   # 检查 sudo 权限
   sudo -v
   ```

2. **安装依赖**（可选）:
   ```bash
   ./nautilus_critest.sh install-deps
   ```

3. **选择测试模式**:
   ```bash
   # 本地测试 - 功能最完整
   ./nautilus_critest.sh local
   
   # 或者容器化测试 - 环境隔离
   ./nautilus_critest.sh container
   
   # 或者快速测试 - 基础验证
   ./nautilus_critest.sh quick
   ```

4. **查看结果**:
   测试完成后，结果会保存在以下位置：
   - 详细报告: `/tmp/nautilus-critest/reports/critest-report.md`
   - 测试日志: `/tmp/nautilus-critest/reports/critest-output.txt`
   - JSON 结果: `/tmp/nautilus-critest/reports/critest-results.json`

## 测试结果解读

### 成功率标准
- **≥95%**: ✅ 测试通过 - CRI 兼容性良好
- **80-94%**: ⚠️ 测试部分通过 - 存在一些兼容性问题
- **<80%**: ❌ 测试失败 - 存在严重的兼容性问题

### 常见问题

1. **权限问题**:
   ```bash
   # 确保不是 root 用户
   [[ $EUID -ne 0 ]] && echo "正确：非 root 用户" || echo "错误：请切换到非 root 用户"
   
   # 检查 sudo 权限
   sudo -v
   ```

2. **Docker 权限问题**:
   ```bash
   # 添加用户到 docker 组
   sudo usermod -aG docker $USER
   newgrp docker
   ```

3. **网络问题**:
   ```bash
   # 检查代理设置
   echo $HTTP_PROXY
   echo $HTTPS_PROXY
   
   # 测试网络连接
   curl -I https://github.com
   ```

4. **依赖安装失败**:
   ```bash
   # 更新包管理器
   sudo apt-get update
   
   # 手动安装基础依赖
   sudo apt-get install -y curl wget git jq bc build-essential
   ```

## 清理环境

```bash
# 清理所有测试环境
./nautilus_critest.sh clean

# 或者使用其他脚本清理
./docker_critest.sh clean
```

## 故障排除

### 查看日志
```bash
# Nautilus 服务日志
cat /tmp/nautilus-critest/logs/nautilus.log

# critest 测试日志
cat /tmp/nautilus-critest/reports/critest-output.txt
```

### 手动调试
```bash
# 检查 Nautilus 进程
ps aux | grep nautilus

# 检查 CRI socket
ls -la /tmp/nautilus-critest/nautilus.sock

# 测试 CRI 连接
critest --runtime-endpoint=unix:///tmp/nautilus-critest/nautilus.sock --ginkgo.focus="should get runtime version"
```

### 重新开始
```bash
# 完全清理并重新开始
./nautilus_critest.sh clean
sudo pkill -f nautilus
sudo rm -rf /tmp/nautilus-critest
./nautilus_critest.sh local
```

## 脚本特性

### 安全特性
- ✅ 非 root 用户执行
- ✅ 自动权限检查
- ✅ 安全的 sudo 使用
- ✅ 完整的清理机制

### 便利特性
- ✅ 自动依赖安装
- ✅ 智能环境检测
- ✅ 详细的日志记录
- ✅ 友好的错误提示
- ✅ 多种测试模式

### 兼容特性
- ✅ 支持代理环境
- ✅ Docker 权限自适应
- ✅ 网络配置自动化
- ✅ 跨平台兼容

## 贡献指南

如果您发现问题或有改进建议，请：

1. 查看现有的 GitHub Issues
2. 提交详细的问题报告
3. 包含测试日志和环境信息
4. 提供复现步骤

## 许可证

本项目遵循与 Nautilus 主项目相同的许可证。