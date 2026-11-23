#!/bin/bash

# Nautilus CRI 兼容性测试自动化脚本
# 用于自动化执行 critest 测试并生成报告

set -euo pipefail

# 脚本配置
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
TEST_DIR="/tmp/nautilus-critest"
LOG_DIR="$TEST_DIR/logs"
REPORT_DIR="$TEST_DIR/reports"
CONFIG_FILE="$TEST_DIR/nautilus-test.toml"
CRI_SOCKET="unix://$TEST_DIR/nautilus.sock"
PID_FILE="$TEST_DIR/nautilus.pid"

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# 日志函数
log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# 错误处理
error_exit() {
    log_error "$1"
    cleanup
    exit 1
}

# 清理函数
cleanup() {
    log_info "清理测试环境..."
    
    # 停止 Nautilus 进程
    if [[ -f "$PID_FILE" ]]; then
        local pid=$(cat "$PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            log_info "停止 Nautilus 进程 (PID: $pid)"
            kill -TERM "$pid" || true
            sleep 2
            if kill -0 "$pid" 2>/dev/null; then
                kill -KILL "$pid" || true
            fi
        fi
        rm -f "$PID_FILE"
    fi
    
    # 清理容器和 Pod
    if command -v runc >/dev/null 2>&1; then
        log_info "清理残留容器..."
        runc list -f json 2>/dev/null | jq -r '.[].id' 2>/dev/null | while read -r container_id; do
            if [[ -n "$container_id" ]]; then
                runc kill "$container_id" KILL 2>/dev/null || true
                runc delete "$container_id" 2>/dev/null || true
            fi
        done
    fi
    
    # 清理网络接口
    if ip link show nautilus0 >/dev/null 2>&1; then
        log_info "清理网络接口..."
        sudo ip link delete nautilus0 2>/dev/null || true
    fi
    
    # 清理挂载点
    log_info "清理挂载点..."
    mount | grep "$TEST_DIR" | awk '{print $3}' | while read -r mount_point; do
        sudo umount "$mount_point" 2>/dev/null || true
    done
    
    log_success "清理完成"
}

# 信号处理
trap cleanup EXIT INT TERM

# 检查依赖
check_dependencies() {
    log_info "检查系统依赖..."
    
    local missing_deps=()
    
    # 检查必需的命令
    local required_commands=("go" "runc" "jq" "curl")
    for cmd in "${required_commands[@]}"; do
        if ! command -v "$cmd" >/dev/null 2>&1; then
            missing_deps+=("$cmd")
        fi
    done
    
    # 检查 critest
    if ! command -v critest >/dev/null 2>&1; then
        log_warning "critest 未安装，将自动安装"
        install_critest
    fi
    
    # 检查 CNI 插件
    if [[ ! -d "/opt/cni/bin" ]] || [[ -z "$(ls -A /opt/cni/bin 2>/dev/null)" ]]; then
        log_warning "CNI 插件未安装，将自动安装"
        install_cni_plugins
    fi
    
    if [[ ${#missing_deps[@]} -gt 0 ]]; then
        error_exit "缺少依赖: ${missing_deps[*]}"
    fi
    
    log_success "依赖检查完成"
}

# 安装 critest
install_critest() {
    log_info "安装 critest..."
    
    local temp_dir=$(mktemp -d)
    cd "$temp_dir"
    
    git clone --depth 1 https://github.com/kubernetes-sigs/cri-tools.git
    cd cri-tools
    
    make critest
    sudo cp build/bin/linux/amd64/critest /usr/local/bin/
    sudo chmod +x /usr/local/bin/critest
    
    cd /
    rm -rf "$temp_dir"
    
    log_success "critest 安装完成"
}

# 安装 CNI 插件
install_cni_plugins() {
    log_info "安装 CNI 插件..."
    
    local cni_version="v1.3.0"
    local temp_dir=$(mktemp -d)
    
    cd "$temp_dir"
    curl -L "https://github.com/containernetworking/plugins/releases/download/${cni_version}/cni-plugins-linux-amd64-${cni_version}.tgz" -o cni-plugins.tgz
    
    sudo mkdir -p /opt/cni/bin
    sudo tar -C /opt/cni/bin -xzf cni-plugins.tgz
    
    cd /
    rm -rf "$temp_dir"
    
    log_success "CNI 插件安装完成"
}

# 准备测试环境
setup_test_environment() {
    log_info "准备测试环境..."
    
    # 创建测试目录
    mkdir -p "$TEST_DIR" "$LOG_DIR" "$REPORT_DIR"
    mkdir -p "$TEST_DIR/state" "$TEST_DIR/storage" "$TEST_DIR/images" "$TEST_DIR/containers" "$TEST_DIR/tmp" "$TEST_DIR/runc"
    
    # 创建 CNI 配置
    sudo mkdir -p /etc/cni/net.d
    cat > "$TEST_DIR/10-nautilus-bridge.conf" << 'EOF'
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
EOF
    sudo cp "$TEST_DIR/10-nautilus-bridge.conf" /etc/cni/net.d/
    
    # 创建 Nautilus 测试配置
    cat > "$CONFIG_FILE" << EOF
[runtime]
state_dir = "$TEST_DIR/state"
log_level = "debug"
max_containers = 100
default_runtime = "runc"
worker_threads = 2

[runtime.runc]
path = "/usr/bin/runc"
root = "$TEST_DIR/runc"
systemd_cgroup = false

[runtime.conmon]
path = "/usr/bin/conmon-rs"
log_size_max = "10MB"
log_tag = "nautilus-test"

[storage]
root_dir = "$TEST_DIR/storage"
image_dir = "$TEST_DIR/images"
container_dir = "$TEST_DIR/containers"
tmp_dir = "$TEST_DIR/tmp"
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
listen_address = "unix://$TEST_DIR/nautilus.sock"
max_concurrent_streams = 100
request_timeout = "30s"
stream_idle_timeout = "1h"

[monitoring]
enable_metrics = true
metrics_address = "127.0.0.1:9091"
log_directory = "$LOG_DIR"
max_log_size = "50MB"
log_rotation = 3

[security]
enable_selinux = false
enable_apparmor = false
allow_privileged = false
EOF
    
    log_success "测试环境准备完成"
}

# 构建 Nautilus
build_nautilus() {
    log_info "构建 Nautilus..."
    
    cd "$PROJECT_ROOT"
    
    # 构建 release 版本
    cargo build --release
    
    if [[ ! -f "target/release/nautilus" ]]; then
        error_exit "Nautilus 构建失败"
    fi
    
    log_success "Nautilus 构建完成"
}

# 启动 Nautilus
start_nautilus() {
    log_info "启动 Nautilus 服务..."
    
    cd "$PROJECT_ROOT"
    
    # 启动 Nautilus 守护进程
    RUST_LOG=debug ./target/release/nautilus daemon --config "$CONFIG_FILE" > "$LOG_DIR/nautilus.log" 2>&1 &
    local nautilus_pid=$!
    echo "$nautilus_pid" > "$PID_FILE"
    
    # 等待服务启动
    local max_wait=30
    local wait_count=0
    
    while [[ $wait_count -lt $max_wait ]]; do
        if [[ -S "${CRI_SOCKET#unix://}" ]]; then
            log_success "Nautilus 服务启动成功 (PID: $nautilus_pid)"
            return 0
        fi
        
        if ! kill -0 "$nautilus_pid" 2>/dev/null; then
            error_exit "Nautilus 进程意外退出"
        fi
        
        sleep 1
        ((wait_count++))
    done
    
    error_exit "Nautilus 服务启动超时"
}

# 运行 critest
run_critest() {
    log_info "运行 critest 测试..."
    
    local test_output="$REPORT_DIR/critest-output.txt"
    local test_json="$REPORT_DIR/critest-results.json"
    
    # 运行 critest
    if critest --runtime-endpoint="$CRI_SOCKET" --image-endpoint="$CRI_SOCKET" --ginkgo.v --ginkgo.no-color > "$test_output" 2>&1; then
        log_success "critest 测试完成"
    else
        log_warning "critest 测试发现问题，查看详细报告"
    fi
    
    # 解析测试结果
    parse_test_results "$test_output" "$test_json"
}

# 解析测试结果
parse_test_results() {
    local output_file="$1"
    local json_file="$2"
    
    log_info "解析测试结果..."
    
    # 提取测试统计信息
    local total_tests=$(grep -c "\[It\]" "$output_file" || echo "0")
    local passed_tests=$(grep -c "\[PASS\]" "$output_file" || echo "0")
    local failed_tests=$(grep -c "\[FAIL\]" "$output_file" || echo "0")
    local skipped_tests=$(grep -c "\[SKIP\]" "$output_file" || echo "0")
    
    # 生成 JSON 报告
    cat > "$json_file" << EOF
{
  "timestamp": "$(date -Iseconds)",
  "environment": {
    "os": "$(uname -s)",
    "kernel": "$(uname -r)",
    "arch": "$(uname -m)",
    "nautilus_version": "$(cd "$PROJECT_ROOT" && cargo metadata --format-version 1 | jq -r '.packages[] | select(.name == "nautilus") | .version')"
  },
  "results": {
    "total": $total_tests,
    "passed": $passed_tests,
    "failed": $failed_tests,
    "skipped": $skipped_tests,
    "success_rate": $(echo "scale=2; $passed_tests * 100 / $total_tests" | bc -l 2>/dev/null || echo "0")
  },
  "test_output_file": "$output_file"
}
EOF
    
    log_success "测试结果解析完成"
}

# 生成测试报告
generate_report() {
    log_info "生成测试报告..."
    
    local json_file="$REPORT_DIR/critest-results.json"
    local report_file="$REPORT_DIR/critest-report.md"
    
    if [[ ! -f "$json_file" ]]; then
        error_exit "测试结果文件不存在: $json_file"
    fi
    
    # 读取测试结果
    local timestamp=$(jq -r '.timestamp' "$json_file")
    local total=$(jq -r '.results.total' "$json_file")
    local passed=$(jq -r '.results.passed' "$json_file")
    local failed=$(jq -r '.results.failed' "$json_file")
    local skipped=$(jq -r '.results.skipped' "$json_file")
    local success_rate=$(jq -r '.results.success_rate' "$json_file")
    local nautilus_version=$(jq -r '.environment.nautilus_version' "$json_file")
    
    # 生成 Markdown 报告
    cat > "$report_file" << EOF
# Nautilus CRI 兼容性测试报告

## 测试概要

- **测试时间**: $timestamp
- **Nautilus 版本**: $nautilus_version
- **测试环境**: $(uname -s) $(uname -r) $(uname -m)
- **测试工具**: critest

## 测试结果

| 指标 | 数量 | 百分比 |
|------|------|--------|
| 总测试数 | $total | 100% |
| 通过测试 | $passed | $(echo "scale=1; $passed * 100 / $total" | bc -l)% |
| 失败测试 | $failed | $(echo "scale=1; $failed * 100 / $total" | bc -l)% |
| 跳过测试 | $skipped | $(echo "scale=1; $skipped * 100 / $total" | bc -l)% |

**总体成功率**: $success_rate%

## 测试状态

EOF
    
    if (( $(echo "$success_rate >= 95" | bc -l) )); then
        echo "✅ **测试通过** - CRI 兼容性良好" >> "$report_file"
    elif (( $(echo "$success_rate >= 80" | bc -l) )); then
        echo "⚠️ **测试部分通过** - 存在一些兼容性问题" >> "$report_file"
    else
        echo "❌ **测试失败** - 存在严重的兼容性问题" >> "$report_file"
    fi
    
    cat >> "$report_file" << EOF

## 详细日志

完整的测试输出请查看: \`$REPORT_DIR/critest-output.txt\`

## 问题分析

EOF
    
    # 提取失败的测试
    if [[ $failed -gt 0 ]]; then
        echo "### 失败的测试" >> "$report_file"
        echo "" >> "$report_file"
        grep -A 5 "\[FAIL\]" "$REPORT_DIR/critest-output.txt" | head -20 >> "$report_file" || true
        echo "" >> "$report_file"
    fi
    
    # 添加建议
    cat >> "$report_file" << EOF

## 建议

1. 查看详细日志文件了解失败原因
2. 检查 Nautilus 配置是否正确
3. 验证依赖组件是否正常工作
4. 如有问题，请提交 GitHub Issue

---

*报告生成时间: $(date)*
EOF
    
    log_success "测试报告生成完成: $report_file"
}

# 显示结果摘要
show_summary() {
    local json_file="$REPORT_DIR/critest-results.json"
    
    if [[ ! -f "$json_file" ]]; then
        log_error "无法找到测试结果文件"
        return 1
    fi
    
    echo
    echo "==========================================="
    echo "           Nautilus CRI 测试摘要"
    echo "==========================================="
    echo
    
    local total=$(jq -r '.results.total' "$json_file")
    local passed=$(jq -r '.results.passed' "$json_file")
    local failed=$(jq -r '.results.failed' "$json_file")
    local skipped=$(jq -r '.results.skipped' "$json_file")
    local success_rate=$(jq -r '.results.success_rate' "$json_file")
    
    echo "总测试数: $total"
    echo "通过测试: $passed"
    echo "失败测试: $failed"
    echo "跳过测试: $skipped"
    echo "成功率: $success_rate%"
    echo
    
    if (( $(echo "$success_rate >= 95" | bc -l) )); then
        log_success "CRI 兼容性测试通过！"
    elif (( $(echo "$success_rate >= 80" | bc -l) )); then
        log_warning "CRI 兼容性测试部分通过，存在一些问题"
    else
        log_error "CRI 兼容性测试失败，存在严重问题"
    fi
    
    echo
    echo "详细报告: $REPORT_DIR/critest-report.md"
    echo "测试日志: $REPORT_DIR/critest-output.txt"
    echo "JSON 结果: $REPORT_DIR/critest-results.json"
    echo
}

# 主函数
main() {
    log_info "开始 Nautilus CRI 兼容性测试"
    
    # 检查是否以 root 权限运行
if [[ $EUID -eq 0 ]]; then
    error_exit "请不要以 root 权限运行此脚本"
fi

# 检查sudo权限
if ! sudo -n true 2>/dev/null; then
    log_warning "需要sudo权限来安装依赖和配置系统"
    log_info "请输入sudo密码以继续..."
    sudo true || error_exit "需要sudo权限才能继续"
fi
    
    # 执行测试流程
    check_dependencies
    setup_test_environment
    build_nautilus
    start_nautilus
    
    # 等待一下确保服务完全启动
    sleep 3
    
    run_critest
    generate_report
    show_summary
    
    log_success "测试完成！"
}

# 脚本入口
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi