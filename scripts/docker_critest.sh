#!/bin/bash

# Nautilus CRI 容器化测试脚本
# 在 Docker 容器中运行 critest 测试

set -euo pipefail

# 脚本配置
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
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
    log_info "清理容器环境..."
    
    # 停止并删除容器
    if ${DOCKER_CMD:-docker} ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
        ${DOCKER_CMD:-docker} stop "$CONTAINER_NAME" >/dev/null 2>&1 || true
        ${DOCKER_CMD:-docker} rm "$CONTAINER_NAME" >/dev/null 2>&1 || true
    fi
    
    log_success "清理完成"
}

# 信号处理
trap cleanup EXIT INT TERM

# 检查 Docker
check_docker() {
    log_info "检查 Docker 环境..."
    
    if ! command -v docker >/dev/null 2>&1; then
        error_exit "Docker 未安装，请先安装 Docker"
    fi
    
    # 检查Docker服务状态
    if ! sudo systemctl is-active --quiet docker 2>/dev/null; then
        log_info "启动 Docker 服务..."
        sudo systemctl start docker || true
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

# 创建 Dockerfile
create_dockerfile() {
    log_info "创建测试容器镜像..."
    
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

# 创建测试脚本
COPY scripts/critest_runner.sh /usr/local/bin/critest_runner.sh
RUN chmod +x /usr/local/bin/critest_runner.sh

# 设置入口点
ENTRYPOINT ["/usr/local/bin/critest_runner.sh"]
EOF
    
    log_success "Dockerfile 创建完成"
}

# 构建测试镜像
build_test_image() {
    log_info "构建测试镜像..."
    
    cd "$PROJECT_ROOT"
    
    # 构建 Docker 镜像（受限网络环境可导出 NAUTILUS_PROXY 提供构建期代理）
    local proxy_args=()
    if [ -n "${NAUTILUS_PROXY:-}" ]; then
        proxy_args=(--build-arg "HTTPS_PROXY=$NAUTILUS_PROXY" --build-arg "HTTP_PROXY=$NAUTILUS_PROXY")
    fi
    if $DOCKER_CMD build --network host "${proxy_args[@]}" -f Dockerfile.critest -t "$DOCKER_IMAGE" .; then
        log_success "测试镜像构建完成"
    else
        error_exit "测试镜像构建失败"
    fi
}

# 运行容器化测试
run_containerized_test() {
    log_info "运行容器化测试..."
    
    # 创建结果目录
    mkdir -p "$TEST_RESULTS_DIR"
    
    # 运行测试容器
    if $DOCKER_CMD run \
        --name "$CONTAINER_NAME" \
        --privileged \
        --pid host \
        --network host \
        --volume "$PROJECT_ROOT:/workspace" \
        --volume "$TEST_RESULTS_DIR:/workspace/test-results" \
        --volume /sys/fs/cgroup:/sys/fs/cgroup:rw \
        --volume /var/run/docker.sock:/var/run/docker.sock \
        --workdir /workspace \
        "$DOCKER_IMAGE"; then
        log_success "容器化测试完成"
    else
        log_warning "容器化测试发现问题，查看详细日志"
    fi
}

# 创建简化的测试脚本
create_simple_test() {
    log_info "创建简化测试脚本..."
    
    cat > "$PROJECT_ROOT/scripts/simple_critest.sh" << 'EOF'
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

# 准备环境
setup_environment() {
    log_info "准备测试环境..."
    
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
EOF
    
    chmod +x "$PROJECT_ROOT/scripts/simple_critest.sh"
    log_success "简化测试脚本创建完成"
}

# 运行本地测试
run_local_test() {
    log_info "运行本地测试..."
    
    cd "$PROJECT_ROOT"
    
    # 检查是否有 Docker 权限
    if ! docker ps >/dev/null 2>&1; then
        log_warning "无 Docker 权限，尝试使用 sudo"
        if sudo docker ps >/dev/null 2>&1; then
            log_info "使用 sudo 运行 Docker 命令"
            DOCKER_CMD="sudo docker"
        else
            error_exit "无法访问 Docker"
        fi
    else
        DOCKER_CMD="docker"
    fi
    
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
            apt-get update && apt-get install -y curl build-essential pkg-config libseccomp-dev git jq bc
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
            bash scripts/simple_critest.sh
        '
    
    log_success "本地测试完成"
}

# 显示帮助
show_help() {
    cat << EOF
Nautilus CRI 容器化测试脚本

用法:
    $0 [选项]

选项:
    build       构建测试镜像
    run         运行容器化测试
    local       运行本地容器测试（推荐）
    clean       清理测试环境
    help        显示此帮助信息

示例:
    $0 local    # 运行本地容器测试
    $0 build    # 构建测试镜像
    $0 run      # 运行完整容器化测试

EOF
}

# 主函数
main() {
    local action="${1:-local}"
    
    case "$action" in
        "build")
            check_docker
            create_dockerfile
            create_simple_test
            build_test_image
            ;;
        "run")
            check_docker
            create_dockerfile
            create_simple_test
            build_test_image
            run_containerized_test
            ;;
        "local")
            check_docker
            create_simple_test
            run_local_test
            ;;
        "clean")
            cleanup
            if [[ -f "$PROJECT_ROOT/Dockerfile.critest" ]]; then
                rm "$PROJECT_ROOT/Dockerfile.critest"
            fi
            if ${DOCKER_CMD:-docker} images -q "$DOCKER_IMAGE" >/dev/null 2>&1; then
                ${DOCKER_CMD:-docker} rmi "$DOCKER_IMAGE" >/dev/null 2>&1 || true
            fi
            log_success "清理完成"
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