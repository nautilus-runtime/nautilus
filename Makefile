# Nautilus 容器运行时 Makefile
# 提供构建、测试、安装和部署的便捷命令

# 默认目标
.DEFAULT_GOAL := help

# 变量定义
CARGO := cargo
TARGET ?= $(if $(CARGO_BUILD_TARGET),$(CARGO_BUILD_TARGET),$(shell uname -m)-unknown-linux-gnu)
CARGO_TARGET_DIR ?= target
RELEASE_DIR = $(CARGO_TARGET_DIR)/$(TARGET)/release
SUDO ?= $(if $(DESTDIR),,sudo)
INSTALL_DIR := /usr/local/bin
CONFIG_DIR := /etc/nautilus
SERVICE_DIR := /etc/systemd/system
DATA_DIR := /var/lib/nautilus
LOG_DIR := /var/log/nautilus
RUN_DIR := /var/run/nautilus

# 构建目标
.PHONY: build
build: ## 构建开发版本
	@echo "🔨 构建 Nautilus 开发版本..."
	$(CARGO) build
	@echo "✅ 构建完成"

.PHONY: release
release: ## 构建生产版本
	@echo "🚀 构建 Nautilus 生产版本..."
	$(CARGO) build --locked --release --target $(TARGET) --target-dir $(CARGO_TARGET_DIR)
	@echo "✅ 生产版本构建完成"

.PHONY: debug
debug: ## 构建调试版本
	@echo "🐛 构建 Nautilus 调试版本..."
	$(CARGO) build --features debug
	@echo "✅ 调试版本构建完成"

# 测试目标
.PHONY: test
test: ## 运行所有测试
	@echo "🧪 运行测试套件..."
	$(CARGO) test
	@echo "✅ 测试完成"

.PHONY: test-unit
test-unit: ## 运行单元测试
	@echo "🔬 运行单元测试..."
	$(CARGO) test --lib

.PHONY: test-integration
test-integration: ## 运行集成测试
	@echo "🔗 运行集成测试..."
	$(CARGO) test --test '*'

.PHONY: test-e2e
test-e2e: ## 运行端到端测试
	@echo "🌐 运行端到端测试..."
	$(CARGO) test --test e2e_lifecycle_test

.PHONY: test-coverage
test-coverage: ## 生成测试覆盖率报告
	@echo "📊 生成测试覆盖率报告..."
	$(CARGO) tarpaulin --out Html --output-dir coverage

# 代码质量
.PHONY: lint
lint: ## 运行代码检查
	@echo "🔍 运行代码检查..."
	$(CARGO) clippy -- -D warnings
	@echo "✅ 代码检查完成"

.PHONY: fmt
fmt: ## 格式化代码
	@echo "🎨 格式化代码..."
	$(CARGO) fmt
	@echo "✅ 代码格式化完成"

.PHONY: fmt-check
fmt-check: ## 检查代码格式
	@echo "📝 检查代码格式..."
	$(CARGO) fmt -- --check

.PHONY: audit
audit: ## 安全审计
	@echo "🔒 运行安全审计..."
	$(CARGO) audit
	@echo "✅ 安全审计完成"

# 文档
.PHONY: doc
doc: ## 生成文档
	@echo "📚 生成文档..."
	$(CARGO) doc --no-deps --open

.PHONY: doc-all
doc-all: ## 生成完整文档
	@echo "📖 生成完整文档..."
	$(CARGO) doc --open

# 安装和部署
.PHONY: install
install: release ## 安装到系统
	$(SUDO) install -d "$(DESTDIR)$(INSTALL_DIR)" "$(DESTDIR)$(CONFIG_DIR)" "$(DESTDIR)$(DATA_DIR)" "$(DESTDIR)$(LOG_DIR)" "$(DESTDIR)$(RUN_DIR)"
	$(SUDO) install -m 0755 "$(RELEASE_DIR)/nautilus" "$(RELEASE_DIR)/nautilusd" "$(DESTDIR)$(INSTALL_DIR)/"
	@test -e "$(DESTDIR)$(CONFIG_DIR)/config.toml" || $(SUDO) install -m 0644 nautilus.toml "$(DESTDIR)$(CONFIG_DIR)/config.toml"

.PHONY: install-service
install-service: install ## 安装 systemd 服务
	$(SUDO) install -d "$(DESTDIR)$(SERVICE_DIR)"
	@set -eu; unit=$$(mktemp); trap 'rm -f "$$unit"' EXIT; \
		sed -e 's|@BINDIR@|$(INSTALL_DIR)|g' -e 's|@CONFDIR@|$(CONFIG_DIR)|g' \
			packaging/nautilusd.service.in > "$$unit"; \
		$(SUDO) install -m 0644 "$$unit" "$(DESTDIR)$(SERVICE_DIR)/nautilusd.service"
ifeq ($(DESTDIR),)
	$(SUDO) systemctl daemon-reload
endif

.PHONY: uninstall
uninstall: ## 卸载系统安装，保留配置文件与运行时数据
ifeq ($(DESTDIR),)
	$(SUDO) systemctl disable --now nautilusd.service
endif
	$(SUDO) rm -f "$(DESTDIR)$(SERVICE_DIR)/nautilusd.service" "$(DESTDIR)$(INSTALL_DIR)/nautilus" "$(DESTDIR)$(INSTALL_DIR)/nautilusd"
ifeq ($(DESTDIR),)
	$(SUDO) systemctl daemon-reload
endif

# 开发工具
.PHONY: dev
dev: ## 启动开发模式
	@echo "🔧 启动开发模式..."
	$(CARGO) run -- daemon --config nautilus.toml --log-level debug

.PHONY: watch
watch: ## 监视文件变化并重新构建
	@echo "👀 监视文件变化..."
	$(CARGO) watch -x build

.PHONY: bench
bench: ## 运行性能基准测试
	@echo "⚡ 运行性能基准测试..."
	$(CARGO) bench

# 打包和发布
.PHONY: package
package: release ## 创建发布包
	@echo "📦 创建发布包..."
	mkdir -p dist
	tar -czf dist/nautilus-$(shell uname -m).tar.gz -C target/release nautilus
	cp nautilus.toml dist/
	cp docs/installation_and_usage.md dist/README.md
	@echo "✅ 发布包创建完成: dist/nautilus-$(shell uname -m).tar.gz"

.PHONY: docker
docker: ## 构建 Docker 镜像
	@echo "🐳 构建 Docker 镜像..."
	docker build -t nautilus:latest .
	@echo "✅ Docker 镜像构建完成"

# 清理
.PHONY: clean
clean: ## 清理构建产物
	@echo "🧹 清理构建产物..."
	$(CARGO) clean
	rm -rf dist coverage
	@echo "✅ 清理完成"

.PHONY: clean-all
clean-all: clean ## 深度清理
	@echo "🧽 深度清理..."
	rm -rf target Cargo.lock
	@echo "✅ 深度清理完成"

# 服务管理
.PHONY: start
start: ## 启动服务
	@echo "▶️ 启动 Nautilus 服务..."
	sudo systemctl start nautilusd.service
	@echo "✅ 服务已启动"

.PHONY: stop
stop: ## 停止服务
	@echo "⏹️ 停止 Nautilus 服务..."
	sudo systemctl stop nautilusd.service
	@echo "✅ 服务已停止"

.PHONY: restart
restart: ## 重启服务
	@echo "🔄 重启 Nautilus 服务..."
	sudo systemctl restart nautilusd.service
	@echo "✅ 服务已重启"

.PHONY: status
status: ## 查看服务状态
	@echo "📊 Nautilus 服务状态:"
	sudo systemctl status nautilusd.service

.PHONY: logs
logs: ## 查看服务日志
	@echo "📋 Nautilus 服务日志:"
	sudo journalctl -u nautilusd.service -f

# 验证和检查
.PHONY: check
check: fmt-check lint test ## 运行所有检查
	@echo "✅ 所有检查通过"

.PHONY: verify
verify: ## 验证安装
	@echo "🔍 验证 Nautilus 安装..."
	nautilus version
	nautilus health || echo "健康检查失败，请检查配置"
	@echo "✅ 验证完成"

.PHONY: deps
deps: ## 检查依赖
	@echo "📋 检查系统依赖..."
	@command -v runc >/dev/null 2>&1 || echo "❌ runc 未安装"
	@command -v conmon-rs >/dev/null 2>&1 || echo "❌ conmon-rs 未安装"
	@ls /opt/cni/bin/ >/dev/null 2>&1 || echo "❌ CNI 插件未安装"
	@echo "✅ 依赖检查完成"

# 帮助
.PHONY: help
help: ## 显示帮助信息
	@echo "Nautilus 容器运行时 Makefile"
	@echo ""
	@echo "可用命令:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "示例:"
	@echo "  make build          # 构建开发版本"
	@echo "  make test           # 运行测试"
	@echo "  make install        # 安装到系统"
	@echo "  make install-service # 安装 systemd 服务"
	@echo "  make start          # 启动服务"
	@echo "  make logs           # 查看日志"
