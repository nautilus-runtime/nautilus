#!/bin/bash

# 简化的 CRI 测试脚本
# 适用于容器环境

set -euo pipefail

# 配置
TEST_DIR="/tmp/nautilus-test"
CONFIG_FILE="$TEST_DIR/nautilus.toml"
CRI_SOCKET="unix://$TEST_DIR/nautilus.sock"
PID_FILE="$TEST_DIR/nautilus.pid"

# 日志函数
log_info() {
    echo "[INFO] $1"
}

log_error() {
    echo "[ERROR] $1" >&2
}

# 清理函数
cleanup() {
    if [[ -f "$PID_FILE" ]]; then
        local pid=$(cat "$PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" || true
            sleep 2
            if kill -0 "$pid" 2>/dev/null; then
                kill -KILL "$pid" || true
            fi
        fi
        rm -f "$PID_FILE"
    fi
}

trap cleanup EXIT

# 准备测试环境
setup_environment() {
    log_info "准备测试环境..."
    
    mkdir -p "$TEST_DIR/state" "$TEST_DIR/storage" "$TEST_DIR/logs"
    
    # 创建 CNI 配置
    sudo mkdir -p /etc/cni/net.d
    cat > "$TEST_DIR/10-bridge.conf" << 'EOF'
{
  "cniVersion": "1.0.0",
  "name": "bridge",
  "type": "bridge",
  "bridge": "cni0",
  "isGateway": true,
  "ipMasq": true,
  "ipam": {
    "type": "host-local",
    "subnet": "10.88.0.0/16",
    "gateway": "10.88.0.1"
  }
}
EOF
    sudo cp "$TEST_DIR/10-bridge.conf" /etc/cni/net.d/10-bridge.conf
    
    # 创建 Nautilus 配置
    cat > "$CONFIG_FILE" << CONFIGEOF
[runtime]
state_dir = "$TEST_DIR/state"
log_level = "debug"
max_containers = 50
default_runtime = "runc"

[runtime.runc]
path = "/usr/bin/runc"
root = "$TEST_DIR/runc"
systemd_cgroup = false

[storage]
root_dir = "$TEST_DIR/storage"
image_dir = "$TEST_DIR/images"
container_dir = "$TEST_DIR/containers"
tmp_dir = "$TEST_DIR/tmp"

[network]
cni_config_dir = "/etc/cni/net.d"
cni_bin_dir = "/opt/cni/bin"
default_network = "bridge"

[cri]
listen_address = "unix://$TEST_DIR/nautilus.sock"
max_concurrent_streams = 50
request_timeout = "30s"

[monitoring]
enable_metrics = false
log_directory = "$TEST_DIR/logs"
CONFIGEOF
}

# 构建 Nautilus
build_nautilus() {
    log_info "构建 Nautilus..."
    
    cd /workspace
    cargo build --release
    
    if [[ ! -f "target/release/nautilus" ]]; then
        log_error "Nautilus 构建失败"
        exit 1
    fi
}

# 启动 Nautilus
start_nautilus() {
    log_info "启动 Nautilus..."
    
    cd /workspace
    
    # 启动服务
    RUST_LOG=debug ./target/release/nautilus daemon --config "$CONFIG_FILE" > "$TEST_DIR/logs/nautilus.log" 2>&1 &
    local pid=$!
    echo "$pid" > "$PID_FILE"
    
    # 等待启动
    local count=0
    while [[ $count -lt 30 ]]; do
        if [[ -S "${CRI_SOCKET#unix://}" ]]; then
            log_info "Nautilus 启动成功"
            return 0
        fi
        sleep 1
        ((count++))
    done
    
    log_error "Nautilus 启动超时"
    exit 1
}

# 运行基础测试
run_basic_tests() {
    log_info "运行基础 CRI 测试..."
    
    # 测试连接
    if critest --runtime-endpoint="$CRI_SOCKET" --image-endpoint="$CRI_SOCKET" --ginkgo.focus="should get runtime version" --ginkgo.v; then
        log_info "✓ 版本查询测试通过"
    else
        log_error "✗ 版本查询测试失败"
    fi
    
    # 测试运行时状态
    if critest --runtime-endpoint="$CRI_SOCKET" --image-endpoint="$CRI_SOCKET" --ginkgo.focus="should get runtime status" --ginkgo.v; then
        log_info "✓ 运行时状态测试通过"
    else
        log_error "✗ 运行时状态测试失败"
    fi
}

# 运行完整测试
run_full_tests() {
    log_info "运行完整 CRI 测试..."
    
    local output_file="/workspace/test-results/critest-output.txt"
    mkdir -p "$(dirname "$output_file")"
    
    if critest --runtime-endpoint="$CRI_SOCKET" --image-endpoint="$CRI_SOCKET" --ginkgo.v > "$output_file" 2>&1; then
        log_info "✓ 完整测试通过"
    else
        log_info "完整测试完成，查看详细结果"
    fi
    
    # 生成简单报告
    local total=$(grep -c "\[It\]" "$output_file" || echo "0")
    local passed=$(grep -c "\[PASS\]" "$output_file" || echo "0")
    local failed=$(grep -c "\[FAIL\]" "$output_file" || echo "0")
    
    echo "==========================================="
    echo "           CRI 测试结果摘要"
    echo "==========================================="
    echo "总测试数: $total"
    echo "通过测试: $passed"
    echo "失败测试: $failed"
    echo "成功率: $(echo "scale=1; $passed * 100 / $total" | bc -l 2>/dev/null || echo "N/A")%"
    echo "详细日志: $output_file"
    echo "==========================================="
}

# 主函数
main() {
    setup_environment
    build_nautilus
    start_nautilus
    
    # 等待服务稳定
    sleep 3
    
    # 根据参数选择测试类型
    if [[ "${1:-full}" == "basic" ]]; then
        run_basic_tests
    else
        run_full_tests
    fi
}

main "$@"
