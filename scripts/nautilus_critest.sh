#!/bin/bash

# Nautilus CRI 测试主脚本
# 支持本地测试和容器化测试，适用于非root用户

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
DOCKER_IMAGE="nautilus-critest:latest"
CONTAINER_NAME="nautilus-critest-runner"
TEST_RESULTS_DIR="$PROJECT_ROOT/test-results"

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

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
    
    # 清理Docker容器
    if command -v docker >/dev/null 2>&1; then
        if docker ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
            docker stop "$CONTAINER_NAME" >/dev/null 2>&1 || true
            docker rm "$CONTAINER_NAME" >/dev/null 2>&1 || true
        fi
    fi
    
    log_success "清理完成"
}

# 信号处理
trap cleanup EXIT INT TERM

# 检查用户权限
check_user_permissions() {
    if [[ $EUID -eq 0 ]]; then
        error_exit "请不要以 root 权限运行此脚本"
    fi
    
    # 检查sudo权限
    if ! sudo -n true 2>/dev/null; then
        log_warning "需要sudo权限来安装依赖和配置系统"
        log_info "请输入sudo密码以继续..."
        sudo true || error_exit "需要sudo权限才能继续"
    fi
    
    log_success "用户权限检查完成"
}

# 检查系统依赖
check_system_dependencies() {
    log_info "检查系统依赖..."
    
    local missing_deps=()
    local required_commands=("curl" "wget" "git" "jq" "bc" "tar" "make")
    
    for cmd in "${required_commands[@]}"; do
        if ! command -v "$cmd" >/dev/null 2>&1; then
            missing_deps+=("$cmd")
        fi
    done
    
    if [[ ${#missing_deps[@]} -gt 0 ]]; then
        log_info "安装缺失的系统依赖: ${missing_deps[*]}"
        sudo apt-get update
        sudo apt-get install -y "${missing_deps[@]}"
    fi
    
    log_success "系统依赖检查完成"
}

# 检查并安装Go
check_and_install_go() {
    if ! command -v go >/dev/null 2>&1; then
        log_info "安装 Go..."
        local go_version="1.21.5"
        local temp_dir=$(mktemp -d)
        cd "$temp_dir"
        
        wget -q "https://go.dev/dl/go${go_version}.linux-amd64.tar.gz"
        sudo tar -C /usr/local -xzf "go${go_version}.linux-amd64.tar.gz"
        
        # 添加到PATH
        if ! grep -q '/usr/local/go/bin' ~/.bashrc; then
            echo 'export PATH=$PATH:/usr/local/go/bin' >> ~/.bashrc
        fi
        export PATH=$PATH:/usr/local/go/bin
        
        cd /
        rm -rf "$temp_dir"
        log_success "Go 安装完成"
    else
        log_info "Go 已安装: $(go version)"
    fi
}

# 检查并安装Rust
check_and_install_rust() {
    if ! command -v cargo >/dev/null 2>&1; then
        log_info "安装 Rust..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        source ~/.cargo/env
        log_success "Rust 安装完成"
    else
        log_info "Rust 已安装: $(rustc --version)"
        source ~/.cargo/env 2>/dev/null || true
    fi
}

# 检查并安装runc
check_and_install_runc() {
    if ! command -v runc >/dev/null 2>&1; then
        log_info "安装 runc..."
        local temp_dir=$(mktemp -d)
        cd "$temp_dir"
        
        wget -q https://github.com/opencontainers/runc/releases/download/v1.1.10/runc.amd64
        sudo mv runc.amd64 /usr/bin/runc
        sudo chmod +x /usr/bin/runc
        
        cd /
        rm -rf "$temp_dir"
        log_success "runc 安装完成"
    else
        log_info "runc 已安装: $(runc --version | head -1)"
    fi
}

# 检查并安装CNI插件
check_and_install_cni() {
    if [[ ! -d "/opt/cni/bin" ]] || [[ -z "$(ls -A /opt/cni/bin 2>/dev/null)" ]]; then
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
    else
        log_info "CNI 插件已安装"
    fi
}

# 检查并安装critest
check_and_install_critest() {
    if ! command -v critest >/dev/null 2>&1; then
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
    else
        log_info "critest 已安装"
    fi
}

# 检查Docker
check_docker() {
    log_info "检查 Docker 环境..."
    
    if ! command -v docker >/dev/null 2>&1; then
        error_exit "Docker 未安装，请先安装 Docker"
    fi
    
    # 检查Docker服务状态
    if ! sudo systemctl is-active --quiet docker; then
        log_info "启动 Docker 服务..."
        sudo systemctl start docker
    fi
    
    # 检查Docker权限
    if ! docker ps >/dev/null 2>&1; then
        log_warning "当前用户无Docker权限，将使用sudo"
        if ! sudo docker ps >/dev/null 2>&1; then
            error_exit "无法访问 Docker"
        fi
        DOCKER_CMD="sudo docker"
    else
        DOCKER_CMD="docker"
    fi
    
    log_success "Docker 环境检查完成"
}

# 准备本地测试环境
setup_local_test_environment() {
    log_info "准备本地测试环境..."
    
    # 创建测试目录
    mkdir -p "$TEST_DIR" "$LOG_DIR" "$REPORT_DIR" "$TEST_RESULTS_DIR"
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
    
    log_success "本地测试环境准备完成"
}

# 构建Nautilus
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

# 启动Nautilus服务
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
            error_exit "Nautilus 进程意外退出，查看日志: $LOG_DIR/nautilus.log"
        fi
        
        sleep 1
        ((wait_count++))
    done
    
    error_exit "Nautilus 服务启动超时"
}

# 运行本地critest测试
run_local_critest() {
    log_info "运行本地 critest 测试..."
    
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
  "test_type": "local",
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

# 创建容器化测试的Dockerfile
create_container_dockerfile() {
    log_info "创建容器化测试 Dockerfile..."
    
    cat > "$PROJECT_ROOT/Dockerfile.critest" << 'EOF'
# Nautilus CRI 测试容器
FROM debian:12-slim

# 设置代理环境变量
ARG HTTP_PROXY
ARG HTTPS_PROXY
ENV HTTP_PROXY=${HTTP_PROXY}
ENV HTTPS_PROXY=${HTTPS_PROXY}

# 配置 apt 重试和超时
RUN echo 'Acquire::Retries "3";' > /etc/apt/apt.conf.d/80-retries && \
    echo 'Acquire::http::Timeout "30";' >> /etc/apt/apt.conf.d/80-retries && \
    echo 'Acquire::https::Timeout "30";' >> /etc/apt/apt.conf.d/80-retries

# 安装系统依赖
RUN apt-get update && apt-get install -y \
    build-essential \
    pkg-config \
    libelf-dev \
    zlib1g-dev \
    libseccomp-dev \
    libsystemd-dev \
    git \
    curl \
    wget \
    jq \
    bc \
    iptables \
    iproute2 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* || \
    (apt-get clean && apt-get update --fix-missing && apt-get install -y \
    build-essential \
    pkg-config \
    libelf-dev \
    zlib1g-dev \
    libseccomp-dev \
    libsystemd-dev \
    git \
    curl \
    wget \
    jq \
    bc \
    iptables \
    iproute2 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*)

# 安装 Go
RUN wget -q https://go.dev/dl/go1.21.5.linux-amd64.tar.gz && \
    tar -C /usr/local -xzf go1.21.5.linux-amd64.tar.gz && \
    rm go1.21.5.linux-amd64.tar.gz
ENV PATH=$PATH:/usr/local/go/bin

# 安装 Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# 安装 runc
RUN wget -q https://github.com/opencontainers/runc/releases/download/v1.1.10/runc.amd64 && \
    mv runc.amd64 /usr/bin/runc && \
    chmod +x /usr/bin/runc

# 安装 CNI 插件
RUN mkdir -p /opt/cni/bin && \
    wget -q https://github.com/containernetworking/plugins/releases/download/v1.3.0/cni-plugins-linux-amd64-v1.3.0.tgz && \
    tar -C /opt/cni/bin -xzf cni-plugins-linux-amd64-v1.3.0.tgz && \
    rm cni-plugins-linux-amd64-v1.3.0.tgz

# 安装 critest
RUN git clone --depth 1 https://github.com/kubernetes-sigs/cri-tools.git /tmp/cri-tools && \
    cd /tmp/cri-tools && \
    make critest && \
    cp build/bin/linux/amd64/critest /usr/local/bin/ && \
    chmod +x /usr/local/bin/critest && \
    rm -rf /tmp/cri-tools

# 创建工作目录
WORKDIR /workspace

# 复制容器内测试脚本
COPY scripts/container_critest.sh /usr/local/bin/container_critest.sh
RUN chmod +x /usr/local/bin/container_critest.sh

# 设置入口点
ENTRYPOINT ["/usr/local/bin/container_critest.sh"]
EOF
    
    log_success "Dockerfile 创建完成"
}

# 创建容器内测试脚本
create_container_test_script() {
    log_info "创建容器内测试脚本..."
    
    cat > "$PROJECT_ROOT/scripts/container_critest.sh" << 'EOF'
#!/bin/bash

# 容器内 CRI 测试脚本

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

# 准备环境
setup_environment() {
    log_info "准备容器内测试环境..."
    
    mkdir -p "$TEST_DIR/state" "$TEST_DIR/storage" "$TEST_DIR/logs"
    
    # 创建 CNI 配置
    mkdir -p /etc/cni/net.d
    cat > /etc/cni/net.d/10-bridge.conf << 'CNIEOF'
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
CNIEOF
    
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

# 运行测试
run_tests() {
    log_info "运行容器内 CRI 测试..."
    
    local test_type="${1:-full}"
    local output_file="/workspace/test-results/critest-output.txt"
    mkdir -p "$(dirname "$output_file")"
    
    if [[ "$test_type" == "basic" ]]; then
        # 基础测试
        log_info "运行基础测试..."
        if critest --runtime-endpoint="$CRI_SOCKET" --image-endpoint="$CRI_SOCKET" --ginkgo.focus="should get runtime version" --ginkgo.v; then
            log_info "✓ 版本查询测试通过"
        else
            log_error "✗ 版本查询测试失败"
        fi
        
        if critest --runtime-endpoint="$CRI_SOCKET" --image-endpoint="$CRI_SOCKET" --ginkgo.focus="should get runtime status" --ginkgo.v; then
            log_info "✓ 运行时状态测试通过"
        else
            log_error "✗ 运行时状态测试失败"
        fi
    else
        # 完整测试
        log_info "运行完整测试..."
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
    fi
}

# 主函数
main() {
    setup_environment
    build_nautilus
    start_nautilus
    
    # 等待服务稳定
    sleep 3
    
    # 运行测试
    run_tests "${1:-full}"
}

main "$@"
EOF
    
    chmod +x "$PROJECT_ROOT/scripts/container_critest.sh"
    log_success "容器内测试脚本创建完成"
}

# 构建容器化测试镜像
build_container_image() {
    log_info "构建容器化测试镜像..."
    
    cd "$PROJECT_ROOT"
    
    # 构建 Docker 镜像（受限网络环境可导出 NAUTILUS_PROXY 提供构建期代理）
    local proxy_args=()
    if [ -n "${NAUTILUS_PROXY:-}" ]; then
        proxy_args=(--build-arg "HTTPS_PROXY=$NAUTILUS_PROXY" --build-arg "HTTP_PROXY=$NAUTILUS_PROXY")
    fi
    if $DOCKER_CMD build --network host "${proxy_args[@]}" -f Dockerfile.critest -t "$DOCKER_IMAGE" .; then
        log_success "容器化测试镜像构建完成"
    else
        error_exit "容器化测试镜像构建失败"
    fi
}

# 运行容器化测试
run_container_test() {
    log_info "运行容器化测试..."
    
    local test_type="${1:-full}"
    
    # 创建结果目录
    mkdir -p "$TEST_RESULTS_DIR"
    
    # 运行测试容器
    if $DOCKER_CMD run \
        --rm \
        --name "$CONTAINER_NAME" \
        --privileged \
        --pid host \
        --network host \
        --volume "$PROJECT_ROOT:/workspace" \
        --volume "$TEST_RESULTS_DIR:/workspace/test-results" \
        --volume /sys/fs/cgroup:/sys/fs/cgroup:rw \
        --workdir /workspace \
        "$DOCKER_IMAGE" "$test_type"; then
        log_success "容器化测试完成"
    else
        log_warning "容器化测试发现问题，查看详细日志"
    fi
}

# 运行快速容器测试（不构建镜像）
run_quick_container_test() {
    log_info "运行快速容器测试..."
    
    local test_type="${1:-basic}"
    
    # 创建结果目录
    mkdir -p "$TEST_RESULTS_DIR"
    
    # 运行测试容器
    $DOCKER_CMD run \
        --rm \
        --network host \
        --name "$CONTAINER_NAME" \
        --privileged \
        --volume "$PROJECT_ROOT:/workspace" \
        --volume "$TEST_RESULTS_DIR:/workspace/test-results" \
        --workdir /workspace \
        debian:12-slim \
        bash -c '
            export DEBIAN_FRONTEND=noninteractive
            apt-get update && apt-get install -y curl build-essential pkg-config libelf-dev zlib1g-dev libseccomp-dev git jq bc wget
            curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
            source ~/.cargo/env
            
            # 安装 runc
            wget -q https://github.com/opencontainers/runc/releases/download/v1.1.10/runc.amd64
            mv runc.amd64 /usr/bin/runc
            chmod +x /usr/bin/runc
            
            # 安装 CNI
            mkdir -p /opt/cni/bin
            wget -q https://github.com/containernetworking/plugins/releases/download/v1.3.0/cni-plugins-linux-amd64-v1.3.0.tgz
            tar -C /opt/cni/bin -xzf cni-plugins-linux-amd64-v1.3.0.tgz
            
            # 安装 Go 和 critest
            wget -q https://go.dev/dl/go1.21.5.linux-amd64.tar.gz
            tar -C /usr/local -xzf go1.21.5.linux-amd64.tar.gz
            export PATH=$PATH:/usr/local/go/bin
            
            git clone --depth 1 https://github.com/kubernetes-sigs/cri-tools.git /tmp/cri-tools
            cd /tmp/cri-tools
            make critest
            cp build/bin/linux/amd64/critest /usr/local/bin/
            
            # 运行测试
            cd /workspace
            bash scripts/container_critest.sh '"$test_type"'
        '
    
    log_success "快速容器测试完成"
}

# 生成测试报告
generate_report() {
    log_info "生成测试报告..."
    
    local json_file="$REPORT_DIR/critest-results.json"
    local report_file="$REPORT_DIR/critest-report.md"
    
    if [[ ! -f "$json_file" ]]; then
        log_warning "测试结果文件不存在: $json_file"
        return 1
    fi
    
    # 读取测试结果
    local timestamp=$(jq -r '.timestamp' "$json_file")
    local test_type=$(jq -r '.test_type // "unknown"' "$json_file")
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
- **测试类型**: $test_type
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
    local test_type=$(jq -r '.test_type // "unknown"' "$json_file")
    
    echo "测试类型: $test_type"
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

# 显示帮助
show_help() {
    cat << EOF
Nautilus CRI 测试主脚本

用法:
    $0 [选项]

选项:
    local           运行本地测试（推荐，完整功能）
    container       运行容器化测试（需要先构建镜像）
    quick           运行快速容器测试（无需构建镜像，基础测试）
    build           仅构建容器化测试镜像
    clean           清理测试环境和镜像
    install-deps    仅安装依赖
    help            显示此帮助信息

测试类型说明:
    - local: 在本地环境直接运行测试，功能最完整
    - container: 在预构建的容器中运行测试，环境隔离
    - quick: 在临时容器中运行基础测试，快速验证

示例:
    $0 local        # 运行本地完整测试（推荐）
    $0 quick        # 运行快速容器测试
    $0 container    # 运行容器化测试
    $0 build        # 构建测试镜像
    $0 clean        # 清理环境

EOF
}

# 清理所有测试环境
clean_all() {
    log_info "清理所有测试环境..."
    
    # 清理容器
    if command -v docker >/dev/null 2>&1; then
        if docker ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
            docker stop "$CONTAINER_NAME" >/dev/null 2>&1 || true
            docker rm "$CONTAINER_NAME" >/dev/null 2>&1 || true
        fi
        
        # 清理镜像
        if docker images -q "$DOCKER_IMAGE" >/dev/null 2>&1; then
            docker rmi "$DOCKER_IMAGE" >/dev/null 2>&1 || true
        fi
    fi
    
    # 清理文件
    if [[ -f "$PROJECT_ROOT/Dockerfile.critest" ]]; then
        rm "$PROJECT_ROOT/Dockerfile.critest"
    fi
    
    if [[ -f "$PROJECT_ROOT/scripts/container_critest.sh" ]]; then
        rm "$PROJECT_ROOT/scripts/container_critest.sh"
    fi
    
    # 清理测试目录
    if [[ -d "$TEST_DIR" ]]; then
        rm -rf "$TEST_DIR"
    fi
    
    log_success "清理完成"
}

# 安装所有依赖
install_dependencies() {
    log_info "安装所有依赖..."
    
    check_user_permissions
    check_system_dependencies
    check_and_install_go
    check_and_install_rust
    check_and_install_runc
    check_and_install_cni
    check_and_install_critest
    
    log_success "所有依赖安装完成"
}

# 主函数
main() {
    local action="${1:-local}"
    
    case "$action" in
        "local")
            check_user_permissions
            check_system_dependencies
            check_and_install_go
            check_and_install_rust
            check_and_install_runc
            check_and_install_cni
            check_and_install_critest
            setup_local_test_environment
            build_nautilus
            start_nautilus
            sleep 3
            run_local_critest
            generate_report
            show_summary
            ;;
        "container")
            check_user_permissions
            check_docker
            create_container_dockerfile
            create_container_test_script
            build_container_image
            run_container_test "full"
            ;;
        "quick")
            check_user_permissions
            check_docker
            create_container_test_script
            run_quick_container_test "basic"
            ;;
        "build")
            check_user_permissions
            check_docker
            create_container_dockerfile
            create_container_test_script
            build_container_image
            ;;
        "clean")
            clean_all
            ;;
        "install-deps")
            install_dependencies
            ;;
        "help")
            show_help
            ;;
        *)
            log_error "未知选项: $action"
            show_help
            exit 1
            ;;
    esac
}

# 脚本入口
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi