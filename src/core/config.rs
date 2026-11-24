use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use crate::core::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub runtime: RuntimeConfig,
    pub storage: StorageConfig,
    pub network: NetworkConfig,
    pub cri: CriConfig,
    pub security: SecurityConfig,
    pub debug: DebugConfig,
    pub environment: EnvironmentConfig,
    pub proxy: ProxyConfig,
    pub registry: RegistryConfig,
    pub auth: AuthConfig,
    pub limits: LimitsConfig,
    /// In-process Wasm policy engine (P1b). Optional; zero cost when disabled.
    #[serde(default = "PolicyConfig::default")]
    pub policy: PolicyConfig,
}

/// In-process Wasm policy engine configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    /// Master switch. When false the engine is never consulted.
    #[serde(default)]
    pub enabled: bool,
    /// Path to a wasm module implementing the nautilus policy ABI:
    /// exports `alloc(i32)->i32`, `evaluate(ptr,len)->i32` (0 = allow),
    /// optional `reason_ptr()->i32` / `reason_len()->i32`.
    #[serde(default)]
    pub path: String,
    /// Which hooks invoke the policy: subset of
    /// ["run_pod_sandbox", "create_container", "start_container"].
    #[serde(default = "default_policy_hooks")]
    pub hooks: Vec<String>,
    /// Fail closed on engine errors (compile/eval failures) when true.
    #[serde(default = "default_policy_fail_closed")]
    pub fail_closed: bool,
    /// Wasmtime fuel budget per evaluation — hard bound on policy compute.
    #[serde(default = "default_policy_fuel")]
    pub fuel_limit: u64,
    /// Wall-clock cap per evaluation (ms). Backstop around the fuel limit.
    #[serde(default = "default_policy_timeout_ms")]
    pub timeout_ms: u64,
}
impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: String::new(),
            hooks: default_policy_hooks(),
            fail_closed: false,
            fuel_limit: default_policy_fuel(),
            timeout_ms: default_policy_timeout_ms(),
        }
    }
}
fn default_policy_hooks() -> Vec<String> {
    vec![
        "run_pod_sandbox".to_string(),
        "create_container".to_string(),
        "start_container".to_string(),
    ]
}
fn default_policy_fail_closed() -> bool {
    false
}
fn default_policy_fuel() -> u64 {
    10_000_000
}
fn default_policy_timeout_ms() -> u64 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub state_dir: PathBuf,
    pub log_level: String,
    pub max_containers: usize,
    pub default_runtime: String,
    pub worker_threads: usize,
    #[serde(default = "default_runc_path")]
    pub runc_path: PathBuf,
    #[serde(default)]
    pub runc: RuncConfig,
    /// Multi-sandbox backend routing (RuntimeClass handler -> backend).
    #[serde(default)]
    pub sandboxes: SandboxesConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuncConfig {
    pub path: PathBuf,
    pub root: PathBuf,
    pub systemd_cgroup: bool,
    /// Container lifecycle driver: "libcontainer" (in-process, default) or
    /// "exec" (shell out to the runc binary, kept as fallback).
    #[serde(default = "default_runc_driver")]
    pub driver: String,
}

fn default_runc_path() -> PathBuf {
    "/usr/bin/runc".into()
}

fn default_runc_driver() -> String {
    "libcontainer".to_string()
}

impl Default for RuncConfig {
    fn default() -> Self {
        Self {
            path: default_runc_path(),
            root: "/run/nautilus/runc".into(),
            systemd_cgroup: true,
            driver: default_runc_driver(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub cni_config_dir: PathBuf,
    pub cni_bin_dir: PathBuf,
    /// Where published pod netns bind-mounts live (holder-process model).
    #[serde(default = "default_netns_dir")]
    pub netns_dir: PathBuf,
    pub default_network: String,
    pub bridge_name: String,
    pub subnet: String,
    pub gateway: String,
    pub dns: DnsConfig,
    pub firewall: FirewallConfig,
}

fn default_netns_dir() -> PathBuf {
    "/var/run/nautilus/netns".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirewallConfig {
    pub enable_iptables: bool,
    pub masquerade: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub root_dir: PathBuf,
    pub image_dir: PathBuf,
    pub container_dir: PathBuf,
    pub tmp_dir: PathBuf,
    /// Optional registry mirror for docker.io images (e.g. "docker.1ms.run").
    /// Empty = pull from upstream. Non-docker.io registries pass through.
    #[serde(default)]
    pub registry_mirror: String,
    pub max_image_size: String,
    pub gc_threshold: f64,
    pub cache_size: String,
    pub overlay: OverlayConfig,
    pub gc: GcConfig,
}

/// Multi-sandbox routing configuration (P0 of the multi-sandbox direction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxesConfig {
    /// CRI runtime_handler used when a request carries an empty handler.
    /// Empty value means the built-in default ("runc").
    #[serde(default)]
    pub default_handler: String,
    /// Register the in-process wasmtime backend under handlers "wasi"/"wasm".
    #[serde(default = "default_true")]
    pub wasm_enabled: bool,
    /// Accept explicit VM workload attempts through CRI on this node.
    #[serde(default)]
    pub vm_workloads_enabled: bool,
    /// Operator-supplied images for the experimental Pod VM lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm: Option<VmConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub vmm: PathBuf,
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    /// Rust virtiofsd executable for a private, writable Pod export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtiofsd: Option<PathBuf>,
}

impl Default for SandboxesConfig {
    fn default() -> Self {
        Self {
            default_handler: String::new(),
            wasm_enabled: true,
            vm_workloads_enabled: false,
            vm: None,
        }
    }
}

fn default_seccomp_profile_root() -> String {
    "/var/lib/kubelet/seccomp".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub mount_options: Vec<String>,
    pub ignore_chown_errors: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcConfig {
    pub enable: bool,
    pub interval: String,
    pub threshold: f64,
    pub max_unused_age: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriConfig {
    pub listen_address: String,
    pub max_concurrent_streams: usize,
    pub request_timeout: String,
    pub stream_idle_timeout: String,
    pub max_recv_message_size: String,
    pub max_send_message_size: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    pub enabled: bool,
    pub enable_selinux: bool,
    pub selinux: bool,
    pub enable_apparmor: bool,
    pub apparmor: bool,
    pub allow_privileged: bool,
    pub read_only_root_filesystem: bool,
    /// Default for OCI `process.noNewPrivileges` (edge workloads rarely need
    /// to escalate). CRI per-container overrides land with the security ctx.
    #[serde(default = "default_true")]
    pub no_new_privileges: bool,
    /// Optional path to an OCI seccomp profiles JSON (e.g. docker's default
    /// profile); embedded into the container spec when set.
    #[serde(default)]
    pub seccomp_profile_path: String,
    /// Root directory for `localhost/<name>` seccomp profile references
    /// (kubelet layout). Default: /var/lib/kubelet/seccomp.
    #[serde(default = "default_seccomp_profile_root")]
    pub seccomp_profile_root: String,
    pub default_capabilities: Vec<String>,
    pub disabled_syscalls: Vec<String>,
    pub seccomp: bool,
    pub policies_dir: PathBuf,
    pub audit: bool,
}


impl SecurityConfig {
    /// Canonical defaults (kept in one place; Config::default and
    /// `Default for SecurityConfig` both route here).
    pub fn with_defaults() -> Self {
        Self {
            enabled: true,
            enable_selinux: false,
            selinux: false,
            enable_apparmor: false,
            apparmor: false,
            allow_privileged: false,
            read_only_root_filesystem: false,
            no_new_privileges: true,
            seccomp_profile_path: String::new(),
            seccomp_profile_root: default_seccomp_profile_root(),
            default_capabilities: vec![
                "CHOWN".to_string(), "DAC_OVERRIDE".to_string(), "FSETID".to_string(),
                "FOWNER".to_string(), "MKNOD".to_string(), "NET_RAW".to_string(),
                "SETGID".to_string(), "SETUID".to_string(), "SETFCAP".to_string(),
                "SETPCAP".to_string(), "NET_BIND_SERVICE".to_string(), "SYS_CHROOT".to_string(),
                "KILL".to_string(), "AUDIT_WRITE".to_string(),
            ],
            disabled_syscalls: vec!["reboot".to_string(), "swapon".to_string(), "swapoff".to_string()],
            seccomp: false,
            policies_dir: "/etc/nautilus/security".into(),
            audit: false,
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugConfig {
    pub enable_pprof: bool,
    pub pprof_address: String,
    pub enable_debug_api: bool,
    pub debug_api_address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentConfig {
    pub default_env: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub http_proxy: String,
    pub https_proxy: String,
    pub no_proxy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryConfig {
    pub default_registry: String,
    pub insecure_registries: Vec<String>,
    pub registry_mirrors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub config_file: PathBuf,
    pub credential_helpers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    pub max_open_files: u64,
    pub max_processes: u64,
    pub max_memory_usage: String,
    pub max_cpu_usage: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodConfig {
    pub id: String,
    pub name: String,
    pub namespace: String,
    pub uid: u32,
    pub gid: u32,
    pub hostname: String,
    pub log_directory: PathBuf,
    pub dns_config: Option<DnsConfig>,
    pub port_mappings: Vec<PortMapping>,
    pub labels: HashMap<String, String>,
    pub annotations: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerConfig {
    pub id: String,
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub args: Vec<String>,
    pub working_dir: String,
    pub env: HashMap<String, String>,
    pub mounts: Vec<Mount>,
    pub labels: HashMap<String, String>,
    pub annotations: HashMap<String, String>,
    pub linux: Option<LinuxConfig>,
    pub log_path: PathBuf,
    pub stdin: bool,
    pub stdin_once: bool,
    pub tty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsConfig {
    pub nameservers: Vec<String>,
    pub search_domains: Vec<String>,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortMapping {
    pub protocol: String,
    pub container_port: u32,
    pub host_port: u32,
    pub host_ip: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mount {
    pub source: String,
    pub destination: String,
    pub mount_type: String,
    pub options: Vec<String>,
    pub readonly: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinuxConfig {
    pub uid: u32,
    pub gid: u32,
    pub additional_gids: Vec<u32>,
    pub capabilities: Vec<String>,
    pub readonly_rootfs: bool,
    pub masked_paths: Vec<String>,
    pub readonly_paths: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            runtime: RuntimeConfig {
                state_dir: "/var/lib/nautilus".into(),
                log_level: "info".to_string(),
                max_containers: 1000,
                default_runtime: "runc".to_string(),
                worker_threads: 4,
                runc_path: default_runc_path(),
                runc: RuncConfig {
                    path: default_runc_path(),
                    root: "/run/nautilus/runc".into(),
                    systemd_cgroup: true,
                    driver: default_runc_driver(),
                },
                sandboxes: SandboxesConfig::default(),
            },
            storage: StorageConfig {
                root_dir: "/var/lib/nautilus/storage".into(),
                image_dir: "/var/lib/nautilus/images".into(),
                container_dir: "/var/lib/nautilus/containers".into(),
                tmp_dir: "/tmp/nautilus".into(),
                registry_mirror: String::new(),
                max_image_size: "10GB".to_string(),
                gc_threshold: 0.8,
                cache_size: "512MB".to_string(),
                overlay: OverlayConfig {
                    enabled: true,
                    mount_options: vec!["nodev".to_string()],
                    ignore_chown_errors: false,
                },
                gc: GcConfig {
                    enable: true,
                    interval: "1h".to_string(),
                    threshold: 0.8,
                    max_unused_age: "24h".to_string(),
                },
            },
            network: NetworkConfig {
                cni_config_dir: "/etc/cni/net.d".into(),
                cni_bin_dir: "/opt/cni/bin".into(),
                netns_dir: default_netns_dir(),
                default_network: "bridge".to_string(),
                bridge_name: "nautilus0".to_string(),
                subnet: "172.20.0.0/16".to_string(),
                gateway: "172.20.0.1".to_string(),
                dns: DnsConfig {
                    nameservers: vec!["8.8.8.8".to_string(), "8.8.4.4".to_string()],
                    search_domains: vec!["cluster.local".to_string()],
                    options: vec!["ndots:5".to_string()],
                },
                firewall: FirewallConfig {
                    enable_iptables: true,
                    masquerade: true,
                },
            },
            cri: CriConfig {
                listen_address: "unix:///var/run/nautilus/nautilus.sock".to_string(),
                max_concurrent_streams: 1000,
                request_timeout: "30s".to_string(),
                stream_idle_timeout: "4h".to_string(),
                max_recv_message_size: "16MB".to_string(),
                max_send_message_size: "16MB".to_string(),
            },
            security: SecurityConfig::default(),
            debug: DebugConfig {
                enable_pprof: false,
                pprof_address: "localhost:6060".to_string(),
                enable_debug_api: false,
                debug_api_address: "localhost:8080".to_string(),
            },
            environment: EnvironmentConfig {
                default_env: vec![
                    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
                    "TERM=xterm".to_string(),
                ],
            },
            proxy: ProxyConfig {
                http_proxy: "".to_string(),
                https_proxy: "".to_string(),
                no_proxy: "localhost,127.0.0.1,::1".to_string(),
            },
            registry: RegistryConfig {
                default_registry: "docker.io".to_string(),
                insecure_registries: vec![],
                registry_mirrors: vec![],
            },
            auth: AuthConfig {
                config_file: "/root/.docker/config.json".into(),
                credential_helpers: vec![],
            },
            limits: LimitsConfig {
                max_open_files: 1048576,
                max_processes: 1048576,
                max_memory_usage: "80%".to_string(),
                max_cpu_usage: "80%".to_string(),
            },
            policy: PolicyConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration from file
    pub fn from_file<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)
            .map_err(|e| crate::core::Error::Config(format!("Failed to parse config: {}", e)))?;
        Ok(config)
    }

    /// Save configuration to file
    pub fn to_file<P: AsRef<std::path::Path>>(&self, path: P) -> Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| crate::core::Error::Config(format!("Failed to serialize config: {}", e)))?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<()> {
        // Validate runtime configuration
        if self.runtime.max_containers == 0 {
            return Err(crate::core::Error::Config("max_containers must be greater than 0".to_string()));
        }
        
        if self.runtime.worker_threads == 0 {
            return Err(crate::core::Error::Config("worker_threads must be greater than 0".to_string()));
        }

        // Validate paths exist
        if !self.runtime.runc.path.exists() {
            return Err(crate::core::Error::Config(format!("runc binary not found at {:?}", self.runtime.runc.path)));
        }

        // Missing CNI bins only degrade pod networking.
        // Validate network configuration
        if !self.network.cni_bin_dir.exists() {
            tracing::warn!(
                path = %self.network.cni_bin_dir.display(),
                "CNI bin directory not found; pod networking is not functional"
            );
        }

        // Validate storage configuration
        if self.storage.gc_threshold < 0.0 || self.storage.gc_threshold > 1.0 {
            return Err(crate::core::Error::Config("gc_threshold must be between 0.0 and 1.0".to_string()));
        }

        // Validate CRI configuration
        if self.cri.max_concurrent_streams == 0 {
            return Err(crate::core::Error::Config("max_concurrent_streams must be greater than 0".to_string()));
        }

        // Validate limits configuration
        if self.limits.max_open_files == 0 {
            return Err(crate::core::Error::Config("max_open_files must be greater than 0".to_string()));
        }

        if self.limits.max_processes == 0 {
            return Err(crate::core::Error::Config("max_processes must be greater than 0".to_string()));
        }

        Ok(())
    }
}
