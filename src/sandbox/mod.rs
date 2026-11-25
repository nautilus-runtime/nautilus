//! Sandbox abstraction: pluggable isolation backends routed by CRI runtime_handler.
//!
//! This is the core extension point of the multi-sandbox direction:
//! each RuntimeClass handler name maps to one [`SandboxBackend`] implementation.
//! P0 ships two backends:
//!   * `runc` – real OCI containers via the runc CLI (libcontainer migration follows)
//!   * `wasi` – WASM workloads executed in-process by wasmtime

pub mod runc_backend;
pub mod spec;
pub mod wasm_backend;
pub(crate) mod vm_backend;

use crate::core::{Config, Error, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

/// The isolation technology backing a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxKind {
    /// OCI containers managed through runc.
    Runc,
    /// OCI workloads inside a Pod VM.
    Vm,
    /// WASM/WASI modules executed in-process by wasmtime.
    Wasm,
}

impl SandboxKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxKind::Runc => "runc",
            SandboxKind::Wasm => "wasm",
            SandboxKind::Vm => "vm",
        }
    }
}

/// CRI resource constraints mapped into the OCI spec (`linux.resources`).
/// Zero/empty values mean "unset".
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResourceLimits {
    pub cpu_period: i64,
    pub cpu_quota: i64,
    pub cpu_shares: i64,
    #[serde(default)]
    pub cpuset_cpus: String,
    #[serde(default)]
    pub cpuset_mems: String,
    pub memory_limit_in_bytes: i64,
    #[serde(default)]
    pub oom_score_adj: i32,
    #[serde(default)]
    pub guest_unified: HashMap<String, String>,
    #[serde(default)]
    pub guest_no_hugepages: Vec<String>,
}

/// Per-container security knobs from CRI LinuxContainerSecurityContext.
/// `None`/false fields fall back to daemon-level `[security]` defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerSecurity {
    pub no_new_privs: bool,
    pub readonly_rootfs: bool,
    /// Path to an OCI seccomp profiles JSON (per-request override).
    pub seccomp_profile_path: String,
    /// AppArmor profile name to enforce ("" = unconfined/none).
    #[serde(default)]
    pub apparmor_profile: String,
    /// Run as non-root: (uid, gid) and optionale user name.
    pub run_as_uid: Option<u32>,
    pub run_as_gid: Option<u32>,
    /// umask etc. CRI user name (RunAsUserName).
    pub run_as_name: String,
    /// Supplementary groups added to the process.
    pub supplemental_groups: Vec<u32>,
    /// Effective capabilities additions beyond the default set.
    pub add_capabilities: Vec<String>,
    /// Capabilities removed from the default set; "ALL" drops everything.
    pub drop_capabilities: Vec<String>,
    /// Privileged container: all caps, no seccomp/nnp restrictions.
    pub privileged: bool,
    /// Paths masked (bind /dev/null) in the container filesystem.
    pub masked_paths: Vec<String>,
    /// Paths made read-only in the container filesystem.
    pub readonly_paths: Vec<String>,
    /// Host namespace modes from CRI NamespaceOption (network/pid/ipc).
    pub host_network: bool,
    pub host_pid: bool,
    pub host_ipc: bool,
    /// CRI pid namespace mode == POD (the default): join the pod's shared
    /// PID namespace instead of creating a fresh one per container.
    #[serde(default)]
    pub share_pod_pid: bool,
}

impl ResourceLimits {
    /// True when the caller asked for no constraint at all.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Everything a backend needs to materialize one workload.
#[derive(Debug, Clone)]
pub struct WorkloadSpec {
    /// Unique workload id (nautilus container id).
    pub id: String,
    /// OCI image reference or local WASI module path.
    pub image_ref: String,
    /// Config digest persisted after preparing an OCI image, empty for local modules.
    pub image_id: String,
    pub cgroup_parent: String,
    /// Process arguments requested by the CRI caller.
    /// Merge semantics (docker/k8s compatible), applied by the backend:
    ///   entrypoint non-empty → [entrypoint…, args…]
    ///   else args empty      → image entrypoint + image cmd
    ///   else image has entrypoint → [image-entrypoint…, args…]
    ///   else                 → [args…] (replaces image cmd)
    pub args: Vec<String>,
    pub working_dir: String,
    pub strict_groups: bool,
    pub tty: bool,
    pub stdin_once: bool,
    /// Entrypoint override from CRI `command` (empty = use image default).
    pub entrypoint: Vec<String>,
    /// Extra environment variables.
    pub env: HashMap<String, String>,
    /// Host directory used by backends to expose workload output (best-effort).
    pub output_dir: std::path::PathBuf,
    /// Optional cgroup/resource limits from the CRI request.
    pub resources: Option<ResourceLimits>,
    /// Per-container security overrides (CRI security context).
    pub security: Option<ContainerSecurity>,
    /// Pod network namespace path (containers join it); None = host network.
    pub network_ns: Option<String>,
    /// Pod IPC namespace path when pod sysctls required a dedicated one;
    /// None = container gets its own fresh IPC namespace.
    pub ipc_ns: Option<String>,
    /// Relative path (inside the log-capture mount) where stdout/stderr are
    /// appended; "out.log" is the legacy default.
    pub log_relpath: String,
    /// Sandbox hostname (CRI PodSandboxConfig.hostname).
    pub hostname: Option<String>,
    /// DNS nameserver entries (CRI DNSConfig.servers).
    pub dns_servers: Vec<String>,
    /// DNS search domains (CRI DNSConfig.searches).
    pub dns_searches: Vec<String>,
    /// DNS resolver options (CRI DNSConfig.options).
    pub dns_options: Vec<String>,
    /// Host bind mounts from CRI ContainerConfig.mounts.
    pub volumes: Vec<VolumeMount>,
    /// Path of the pod's shared PID namespace to join (runc pods with a pod
    /// pidns holder); None = fresh pid namespace per container.
    pub pid_ns: Option<String>,
    /// Keep the init's stdin open (CRI ContainerConfig.stdin): the daemon
    /// parks the pipe's write half so stdin-reading workloads block instead
    /// of EOF-exiting before a client attaches.
    pub open_stdin: bool,
}

/// A CRI host-path volume mount.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VolumeMount {
    pub host_path: std::path::PathBuf,
    pub container_path: String,
    pub readonly: bool,
    pub propagation: String,
}

/// Result of starting a workload.
#[derive(Debug)]
pub enum StartOutcome {
    /// Long-running workload: OS pid while it keeps running.
    Detached { pid: u32 },
    /// Guest-owned process, observed using its authenticated agent.
    Guest,
    /// In-process workload held until its Running state is committed.
    Managed {
        activate: tokio::sync::oneshot::Sender<()>,
        finished: tokio::sync::watch::Receiver<Option<i32>>,
    },
}

/// Live stdio handles for a streaming CRI exec/attach session.
///
/// `stdout`/`stderr` yield the process output as it is produced (unlike
/// `exec_sync`, which captures into files); `exit` resolves once the process
/// has been reaped. `pid` lets the streaming server SIGKILL the process when
/// the SPDY connection goes away (a detached exec with no client would
/// otherwise leak in the container namespaces).
pub struct ExecStreams {
    /// Send commands here when the session requested stdin; a dedicated
    /// writer thread forwards them to the process. Dropping the sender closes
    /// the process's stdin (client EOF semantics).
    pub stdin: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    /// Live output queues (fed by dedicated reader threads) until EOF.
    pub stdout: tokio::sync::mpsc::Receiver<Vec<u8>>,
    pub stderr: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// OS pid of the exec'd process; 0 until the backend has spawned it
    /// (spawn happens on a blocking worker after this struct is returned).
    pub pid: std::sync::Arc<std::sync::atomic::AtomicU32>,
    pub exit: tokio::sync::oneshot::Receiver<Result<i32>>,
}

/// A pluggable isolation backend behind a RuntimeClass handler.
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    /// Kind tag of this backend.
    fn kind(&self) -> SandboxKind;

    /// Prepare resources at CRI-create time: fetch/unpack images, build the OCI
    /// bundle, compile the wasm module... Must fail fast on unusable inputs.
    async fn prepare(&self, spec: &WorkloadSpec) -> Result<()>;

    /// Prepare once and return the image identity used by this container. An
    /// OCI backend obtains this from the same manifest that supplied its rootfs.
    async fn prepare_image(&self, spec: &WorkloadSpec) -> Result<String> {
        self.prepare(spec).await?;
        Ok(spec.image_ref.clone())
    }

    /// Start the workload.
    async fn start(&self, spec: &WorkloadSpec) -> Result<StartOutcome>;

    /// Stop a running workload. Returns its exit code when obtainable.
    async fn stop(&self, id: &str, grace: std::time::Duration) -> Result<Option<i32>>;

    /// Run a command inside a RUNNING workload, capturing its streams.
    /// Returns (exit_code, stdout, stderr). `timeout` follows CRI execSync
    /// semantics: when it lapses the exec'd process must be KILLED and
    /// reaped (a dropped future alone leaks it), and the call errors.
    /// Backends that cannot exec (e.g. in-process wasm) inherit this
    /// unsupported default.
    async fn exec_sync(
        &self,
        _id: &str,
        _args: &[String],
        _env: &[(String, String)],
        _cwd: Option<&str>,
        _timeout: Option<std::time::Duration>,
    ) -> Result<(i32, Vec<u8>, Vec<u8>)> {
        Err(Error::Runtime(
            "exec_sync is not supported by this sandbox backend".to_string(),
        ))
    }

    /// Run a command inside a RUNNING workload with live stdio pipes, for the
    /// CRI streaming exec/attach interfaces. The process keeps running after
    /// this call returns; the caller drives it through `ExecStreams` and must
    /// kill `pid` when the client disconnects.
    async fn exec_stream(
        &self,
        _id: &str,
        _args: &[String],
        _env: &[(String, String)],
        _cwd: Option<&str>,
    ) -> Result<ExecStreams> {
        Err(Error::Runtime(
            "exec_stream is not supported by this sandbox backend".to_string(),
        ))
    }

    /// Release every resource owned by the workload (bundles, temp dirs).
    async fn remove(&self, id: &str) -> Result<()>;
}

/// Maps CRI `runtime_handler` names to concrete backends.
pub struct SandboxRegistry {
    default_handler: String,
    handlers: HashMap<String, Arc<dyn SandboxBackend>>,
}

impl SandboxRegistry {
    /// Build the registry from configuration.
    ///
    /// Known handlers:
    ///   * `"runc"` / `""` → default runc backend
    ///   * `"wasi"` / `"wasm"` → wasmtime backend (when `sandboxes.wasm_enabled`)
    pub fn new(config: &Arc<Config>) -> Result<Self> {
        let sb_cfg = &config.runtime.sandboxes;
        let default_handler = if sb_cfg.default_handler.is_empty() {
            "runc".to_string()
        } else {
            sb_cfg.default_handler.clone()
        };

        let mut handlers: HashMap<String, Arc<dyn SandboxBackend>> = HashMap::new();

        let runc = Arc::new(runc_backend::RuncBackend::new(config.clone()));
        handlers.insert("runc".to_string(), runc.clone());
        handlers.insert("".to_string(), runc);

        if sb_cfg.wasm_enabled {
            let wasm = Arc::new(wasm_backend::WasmBackend::new(config.clone())?);
            handlers.insert("wasi".to_string(), wasm.clone());
            handlers.insert("wasm".to_string(), wasm);
        }

        if !handlers.contains_key(&default_handler) {
            return Err(Error::Config(format!(
                "default sandbox handler '{}' has no registered backend",
                default_handler
            )));
        }

        Ok(Self {
            default_handler,
            handlers,
        })
    }

    /// Resolve a CRI runtime_handler (empty selects the default). Reject unknown
    /// handlers early, mirroring the CRI spec wording for RunPodSandboxRequest.
    pub fn resolve(&self, runtime_handler: &str) -> Result<Arc<dyn SandboxBackend>> {
        let key = if runtime_handler.is_empty() {
            self.default_handler.as_str()
        } else {
            runtime_handler
        };
        self.handlers.get(key).cloned().ok_or_else(|| {
            Error::Cri(format!(
                "unknown runtime handler '{}', known handlers: {:?}",
                key,
                self.list_handlers()
            ))
        })
    }

    /// Canonicalize: empty input becomes the effective default handler name.
    pub fn canonical(&self, runtime_handler: &str) -> String {
        if runtime_handler.is_empty() {
            self.default_handler.clone()
        } else {
            runtime_handler.to_string()
        }
    }

    /// All registered handler names (without the empty-string alias).
    pub fn list_handlers(&self) -> Vec<String> {
        let mut names: Vec<String> =
            self.handlers.keys().filter(|k| !k.is_empty()).cloned().collect();
        names.sort();
        names
    }
}
