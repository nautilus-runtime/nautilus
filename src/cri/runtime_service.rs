mod metrics;
mod stats;

use super::list_stream::{batches, ListStream};
use crate::cri::version;
use crate::core::{Config, StateManager};
use crate::runtime::{ContainerManager, PodManager};
use crate::network::PodNet;
use crate::core::state::{ContainerStatus, PodStatus};
use crate::sandbox::SandboxRegistry;

use crate::proto::runtime::v1::runtime_service_server::RuntimeService;
use crate::proto::runtime::v1::*;
use crate::proto::runtime;
use futures::{Stream, StreamExt};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// CRI MountPropagation (proto i32) → OCI bind-mount propagation option.
/// Unknown values fall back to rprivate, the CRI default.
fn cri_propagation_to_oci(p: i32) -> String {
    match crate::proto::runtime::v1::MountPropagation::try_from(p) {
        Ok(crate::proto::runtime::v1::MountPropagation::PropagationHostToContainer) => {
            "rslave".to_string()
        }
        Ok(crate::proto::runtime::v1::MountPropagation::PropagationBidirectional) => {
            "rshared".to_string()
        }
        _ => "rprivate".to_string(),
    }
}

/// Inverse of cri_propagation_to_oci, for reporting mounts in
/// ContainerStatus.
fn oci_propagation_to_cri(oci: &str) -> i32 {
    match oci {
        "rslave" => crate::proto::runtime::v1::MountPropagation::PropagationHostToContainer as i32,
        "rshared" => {
            crate::proto::runtime::v1::MountPropagation::PropagationBidirectional as i32
        }
        _ => crate::proto::runtime::v1::MountPropagation::PropagationPrivate as i32,
    }
}

fn sandbox_state(status: &PodStatus) -> PodSandboxState {
    if *status == PodStatus::Running {
        PodSandboxState::SandboxReady
    } else {
        PodSandboxState::SandboxNotready
    }
}

fn matches_sandbox_state(filter: Option<&PodSandboxStateValue>, status: &PodStatus) -> bool {
    filter.is_none_or(|value| value.state == sandbox_state(status) as i32)
}

fn validate_vm_pod(config: &PodSandboxConfig, host: &Config) -> Result<(), (tonic::Code, &'static str)> {
    use crate::proto::runtime::v1::{NamespaceMode, LinuxSandboxSecurityContext};
    if host.runtime.sandboxes.vm.as_ref().and_then(|vm| vm.virtiofsd.as_ref()).is_none() {
        return Err((tonic::Code::FailedPrecondition, "VM workloads require a guest and virtiofsd configuration"));
    }
    if config.windows.is_some() || !config.port_mappings.is_empty() {
        return Err((tonic::Code::Unimplemented, "VM supports Linux Pods without host port mappings"));
    }
    if config.hostname.len() > 64 || !config.hostname.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
        return Err((tonic::Code::InvalidArgument, "invalid guest hostname"));
    }
    let linux = config.linux.as_ref().ok_or((tonic::Code::Unimplemented, "VM requires Linux Pod namespaces"))?;
    if !linux.sysctls.is_empty() {
        return Err((tonic::Code::Unimplemented, "VM Pod sysctls are unsupported"));
    }
    let mut security = linux.security_context.clone().unwrap_or_default();
    let ns = security.namespace_options.take().unwrap_or_default();
    if ns.network != NamespaceMode::Pod as i32 || ns.ipc != NamespaceMode::Pod as i32
        || ns.pid != NamespaceMode::Container as i32 || !ns.target_id.is_empty() || ns.userns_options.as_ref().is_some_and(|user|
            user.mode != NamespaceMode::Node as i32 || !user.uids.is_empty() || !user.gids.is_empty()) {
        return Err((tonic::Code::Unimplemented, "VM requires Pod networking and IPC, private container PID and no user namespace request"));
    }
    // The first sandbox process is the guest's root agent. Node profile
    // references and sandbox credentials cannot be applied to it.
    // Cloud Hypervisor applies its runtime-default seccomp policy to the
    // host sandbox process. A localhost profile cannot be resolved in the VM.
    if security.seccomp.as_ref().is_some_and(|p| p.profile_type == 0 && p.localhost_ref.is_empty()) {
        security.seccomp = None;
    }
    if security.apparmor.as_ref().is_some_and(|p| p.profile_type == 1 && p.localhost_ref.is_empty()) {
        security.apparmor = None;
    }
    if security != LinuxSandboxSecurityContext::default() {
        return Err((tonic::Code::Unimplemented, "unsupported VM sandbox security context"));
    }
    Ok(())
}

#[derive(Default)]
struct VmReadiness {
    ready: std::sync::atomic::AtomicBool,
    recheck: tokio::sync::Notify,
}

pub struct NautilusRuntimeService {
    _config: Arc<Config>,
    _state_manager: Arc<StateManager>,
    vm_ready: Arc<VmReadiness>,
    images: Arc<crate::storage::ImageManager>,
    pod_manager: Arc<PodManager>,
    container_manager: Arc<ContainerManager>,
    /// Routes CRI runtime_handler values to sandbox backends.
    sandbox_registry: Arc<SandboxRegistry>,
    /// Pod-level networking (netns + CNI chain).
    pod_net: Arc<PodNet>,
    /// In-process Wasm policy engine (P1b).
    policy: Arc<crate::policy::PolicyEngine>,
}

fn validate_vm_container(config: &ContainerConfig) -> Result<(), (tonic::Code, &'static str)> {
    let unsupported = |msg| (tonic::Code::Unimplemented, msg);
    if config.windows.is_some()
        || !config.devices.is_empty()
        || !config.cdi_devices.is_empty()
        || config.stop_signal != 0
    {
        return Err(unsupported("VM guest does not support requested devices, stop signal or Windows configuration"));
    }
    for mount in &config.mounts {
        if mount.propagation != MountPropagation::PropagationPrivate as i32
            || mount.recursive_read_only
            || mount.selinux_relabel
            || !mount.uid_mappings.is_empty()
            || !mount.gid_mappings.is_empty()
            || mount.image.is_some()
            || !mount.image_sub_path.is_empty()
            || mount
                .mount_options
                .iter()
                .any(|opt| !matches!(opt.as_str(), "nosuid" | "nodev" | "noexec"))
        {
            return Err(unsupported("VM host volumes require private propagation, unmapped IDs and ordinary bind options"));
        }
        if !std::path::Path::new(&mount.host_path).is_absolute() {
            return Err((
                tonic::Code::InvalidArgument,
                "VM host volume source must be absolute",
            ));
        }
    }
    if config.envs.iter().any(|e| std::str::from_utf8(&e.value).is_err() || e.key.is_empty() || e.key.contains('=')) {
        return Err((tonic::Code::InvalidArgument, "invalid guest environment"));
    }
    if let Some(r) = config.linux.as_ref().and_then(|l| l.resources.as_ref()) {
        if !(-1000..=1000).contains(&r.oom_score_adj) || r.memory_swap_limit_in_bytes != 0 || r.hugepage_limits.iter().any(|page| page.limit != 0 || !matches!(page.page_size.as_str(), "2MB" | "1GB"))
            || r.unified.iter().any(|(k,v)| !matches!((k.as_str(),v.as_str()), ("memory.oom.group","1") | ("memory.swap.max" | "memory.low" | "memory.min","0")))
            || !r.cpuset_cpus.is_empty() || !r.cpuset_mems.is_empty() {
            return Err(unsupported("VM guest supports container CPU shares, period, quota and memory limit"));
        }
    }
    let sc = config.linux.as_ref().and_then(|l| l.security_context.as_ref())
        .ok_or_else(|| unsupported("VM containers require a private PID namespace"))?;
    let ns = sc.namespace_options.as_ref().ok_or_else(|| unsupported("VM containers require an explicit container PID namespace"))?;
    if ns.network != NamespaceMode::Pod as i32 || ns.pid != NamespaceMode::Container as i32 || ns.ipc != NamespaceMode::Pod as i32
        || !ns.target_id.is_empty() || ns.userns_options.as_ref().is_some_and(|u| !u.uids.is_empty() || !u.gids.is_empty() || u.mode != NamespaceMode::Node as i32) {
        return Err(unsupported("VM containers require private PID, shared Pod network and IPC, and unmapped IDs"));
    }
    if sc.privileged || sc.selinux_options.is_some() || sc.capabilities.as_ref().is_some_and(|c| !c.add_ambient_capabilities.is_empty() || !c.add_capabilities.is_empty())
        || !matches!(sc.supplemental_groups_policy, 0 | 1) {
        return Err(unsupported("VM guest cannot apply the requested security configuration"));
    }
    if sc.supplemental_groups.iter().copied().chain(sc.run_as_user.iter().chain(sc.run_as_group.iter()).map(|v| v.value))
        .any(|id| u32::try_from(id).is_err()) {
        return Err((tonic::Code::InvalidArgument, "guest user and group IDs must be unsigned 32-bit values"));
    }
    Ok(())
}

impl NautilusRuntimeService {
    pub fn new(
        config: Arc<Config>,
        state_manager: Arc<StateManager>,
    ) -> Self {
        Self::with_images(config.clone(), state_manager, Arc::new(crate::storage::ImageManager::new(config)))
    }

    pub fn with_images(config: Arc<Config>, state_manager: Arc<StateManager>, images: Arc<crate::storage::ImageManager>) -> Self {
        // Multi-sandbox routing is the backbone: every pod/container op goes
        // through a backend resolved by its runtime_handler.
        let sandbox_registry = Arc::new(
            SandboxRegistry::new(&config).expect("sandbox registry init failed"),
        );

        let container_manager = Arc::new(ContainerManager::new(
            config.clone(),
            state_manager.clone(),
            sandbox_registry.clone(),
        ));

        let pod_manager = Arc::new(PodManager::new(
            config.clone(),
            state_manager.clone(),
            container_manager.clone(),
        ));

        Self {
            _config: config.clone(),
            _state_manager: state_manager,
            images,
            vm_ready: Default::default(),
            pod_manager,
            container_manager,
            sandbox_registry,
            pod_net: Arc::new(PodNet::new(config.clone())),
            policy: Arc::new(crate::policy::PolicyEngine::new(&config)),
        }
    }
}

impl NautilusRuntimeService {
    pub async fn recover_vm_pods(&self) -> crate::core::Result<()> {
        self.pod_manager.recover_vm_pods().await?;
        self.container_manager.recover_guest_containers().await
    }

    pub fn start_vm_readiness_probe(&self) {
        if !self._config.runtime.sandboxes.vm_workloads_enabled || !self._config.runtime.runc.systemd_cgroup {
            return;
        }
        let manager = self.pod_manager.clone();
        let health = self.vm_ready.clone();
        tokio::spawn(async move {
            let mut delay = 5;
            loop {
                match manager.probe_workloads().await {
                    Ok(()) => {
                        health.ready.store(true, std::sync::atomic::Ordering::Release);
                        delay = 5;
                        health.recheck.notified().await;
                    }
                    Err(error) => {
                        health.ready.store(false, std::sync::atomic::Ordering::Release);
                        tracing::warn!(%error, delay, "VM handler readiness probe failed");
                        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                        delay = (delay * 2).min(60);
                    }
                }
            }
        });
    }

    async fn release_pod_resources(
        &self,
        pod_id: &str,
        network: Option<&crate::network::PodNetInfo>,
        pause_pid: Option<i32>,
        ipc_pid: Option<i32>,
    ) -> crate::core::Result<()> {
        crate::network::publish::remove(pod_id).await;
        if let Some(info) = network {
            self.pod_net.teardown(pod_id, info).await?;
            self.pod_manager.clear_pod_network(pod_id).await?;
        }
        if pause_pid.is_some() {
            crate::network::podnet::destroy_pod_pidns(pod_id, pause_pid).await?;
            self.pod_manager.set_pod_pidns(pod_id, None).await?;
        }
        if ipc_pid.is_some() {
            crate::network::podnet::destroy_pod_ipcns(pod_id, ipc_pid).await?;
            self.pod_manager.set_pod_ipc(pod_id, None).await?;
        }
        Ok(())
    }

    // Caller holds the sandbox operation guard across stopping and removal.
    async fn stop_sandbox(&self, id: &str) -> crate::core::Result<()> {
        let Some(pod) = self.pod_manager.get_pod_state(id).await? else {
            return Ok(());
        };
        if pod.status == PodStatus::Running || pod.runtime_handler == "vm" {
            self.pod_manager.stop_pod_locked(id, None).await?;
        }
        let network = pod.network_namespace.zip(pod.netns_holder_pid).map(|(netns_path, holder_pid)| {
            crate::network::PodNetInfo { netns_path, holder_pid, ip: pod.network_ip }
        });
        if pod.runtime_handler == "runc" {
            self.release_pod_resources(id, network.as_ref(), pod.pidns_holder_pid, pod.ipc_holder_pid).await?;
        }
        Ok(())
    }

    /// Process-level accounting via /proc — no cgroup-path coupling, works for
    /// any backend that reports a pid. (cgroup-native stats land with the
    /// metrics pipeline.)
    async fn stats_response(
        &self,
        container_id: &str,
    ) -> Result<crate::proto::runtime::v1::ContainerStats, Status> {
        use crate::proto::runtime::v1 as rt;
        let c = self
            .container_manager
            .get_container_state(container_id)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("container {} not found", container_id)))?;

        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        if c.sandbox_kind == "vm" {
            return self.guest_container_stats(&c).await;
        }
        let (cpu_ns, mem_bytes) = match c.pid {
            Some(pid) if pid > 0 => {
                let stat = tokio::fs::read_to_string(format!("/proc/{}/stat", pid))
                    .await
                    .map_err(|_| {
                        Status::internal("container process not readable (exited?)")
                    })?;
                // Fields after comm (which may contain spaces) split from the
                // last ')' onward: field 14=utime 15=stime in clock ticks.
                let tail = stat.rsplit(')').next().unwrap_or_default();
                let f: Vec<&str> = tail.split_whitespace().collect();
                let ticks_per_sec = 100f64; // sysconf(_SC_CLK_TCK), Linux default
                let utime: f64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let stime: f64 = f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let rss_pages: u64 = f
                    .get(21)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                (
                    ((utime + stime) / ticks_per_sec * 1e9) as u64,
                    rss_pages * 4096,
                )
            }
            _ => return Err(Status::failed_precondition("container is not running")),
        };

        Ok(rt::ContainerStats {
            attributes: Some(stats::container_attributes(&c)),
            cpu: Some(rt::CpuUsage {
                timestamp: now,
                usage_core_nano_seconds: Some(rt::UInt64Value { value: cpu_ns }),
                usage_nano_cores: None,
                psi: None,
            }),
            memory: Some(rt::MemoryUsage {
                timestamp: now,
                working_set_bytes: Some(rt::UInt64Value { value: mem_bytes }),
                available_bytes: None,
                usage_bytes: None,
                rss_bytes: None,
                page_faults: None,
                major_page_faults: None,
                psi: None,
            }),
            writable_layer: None,
            swap: None,
            io: None,
        })
    }

    /// Aggregate pod stats: sums cpu/memory of the sandbox's live containers
    /// (same /proc-based accounting as `stats_response`).
    async fn pod_stats_response(
        &self,
        pod_id: &str,
    ) -> Result<crate::proto::runtime::v1::PodSandboxStats, Status> {
        use crate::proto::runtime::v1 as rt;
        let pod = self
            .pod_manager
            .get_pod_state(pod_id)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("pod {} not found", pod_id)))?;

        if pod.runtime_handler == "vm" {
            return self.guest_pod_stats(&pod).await;
        }

        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let mut containers = Vec::new();
        let mut cpu_total = 0u64;
        let mut mem_total = 0u64;
        for cid in &pod.containers {
            if let Ok(s) = self.stats_response(cid).await {
                if let Some(cpu) = &s.cpu {
                    if let Some(u) = &cpu.usage_core_nano_seconds {
                        cpu_total += u.value;
                    }
                }
                if let Some(mem) = &s.memory {
                    if let Some(w) = &mem.working_set_bytes {
                        mem_total += w.value;
                    }
                }
                containers.push(s);
            }
        }

        Ok(rt::PodSandboxStats {
            attributes: Some(stats::pod_attributes(&pod)),
            linux: Some(rt::LinuxPodSandboxStats {
                cpu: Some(rt::CpuUsage {
                    timestamp: now,
                    usage_core_nano_seconds: Some(rt::UInt64Value { value: cpu_total }),
                    usage_nano_cores: None,
                    psi: None,
                }),
                memory: Some(rt::MemoryUsage {
                    timestamp: now,
                    working_set_bytes: Some(rt::UInt64Value { value: mem_total }),
                    available_bytes: None,
                    usage_bytes: None,
                    rss_bytes: None,
                    page_faults: None,
                    major_page_faults: None,
                    psi: None,
                }),
                network: None,
                process: Some(rt::ProcessUsage {
                    timestamp: now,
                    process_count: Some(rt::UInt64Value {
                        value: containers.len() as u64,
                    }),
                }),
                containers,
                io: None,
            }),
            windows: None,
        })
    }

}

#[tonic::async_trait]
impl RuntimeService for NautilusRuntimeService {
    async fn version(
        &self,
        _request: Request<VersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        version::version(_request).await
    }

    async fn run_pod_sandbox(
        &self,
        request: Request<RunPodSandboxRequest>,
    ) -> Result<Response<RunPodSandboxResponse>, Status> {
        let req = request.into_inner();
        let config = req.config.ok_or_else(|| {
            Status::invalid_argument("Pod sandbox config is required")
        })?;

        let managed_vm = req.runtime_handler == "vm" && self._config.runtime.sandboxes.vm_workloads_enabled;
        if managed_vm {
            if !self._config.runtime.runc.systemd_cgroup {
                return Err(Status::failed_precondition("VM workloads require the systemd cgroup driver"));
            }
            validate_vm_pod(&config, &self._config).map_err(|(code,msg)| Status::new(code,msg))?;
        } else {
            self.sandbox_registry.resolve(&req.runtime_handler).map_err(Status::from)?;
        }
        let metadata = config.metadata.ok_or_else(|| {
            Status::invalid_argument("Pod metadata is required")
        })?;

        // Multi-sandbox routing: resolve the requested RuntimeClass handler up
        // front and reject unknown ones, per the CRI spec contract.
        let handler = self.sandbox_registry.canonical(&req.runtime_handler);

        let vm_resources = if handler == "vm" {
            Some(crate::vm::resources::VmResources::from_cri(config.linux.as_ref()).map_err(|e| {
                if e.kind() == std::io::ErrorKind::Unsupported {
                    Status::unimplemented(e.to_string())
                } else {
                    Status::invalid_argument(e.to_string())
                }
            })?)
        } else {
            None
        };

        // P1b: in-process policy gate before any pod resources are touched.
        {
            use crate::policy::PolicyRequest;
            self.policy
                .enforce(&PolicyRequest {
                    hook: "run_pod_sandbox",
                    pod_id: &metadata.uid,
                    container_id: "",
                    runtime_handler: &handler,
                    image: "",
                    labels: &config.labels,
                    annotations: &config.annotations,
                    args: &[],
                })
                .await
                .map_err(Status::from)?;
        }

        // Sandbox sysctls (runc only): validate up front for fast, clear
        // errors; the holder applies them inside the fresh netns.
        let sysctls: Vec<(String, String)> = if handler == "runc" {
            let map = config
                .linux
                .as_ref()
                .map(|l| l.sysctls.clone())
                .unwrap_or_default();
            let v: Vec<(String, String)> = map.into_iter().collect();
            for (k, _) in &v {
                crate::network::sysctl::validate_sysctl_key(k)
                    .map_err(|e| Status::invalid_argument(e.to_string()))?;
            }
            v
        } else {
            Vec::new()
        };

        // hostNetwork is a pod-scoped request (CRI): no CNI netns for the
        // sandbox, and containers keep the host network stack. Same for
        // pid/ipc NODE modes: containers inherit the host namespaces.
        let node_mode = crate::proto::runtime::v1::NamespaceMode::Node as i32;
        let pod_ns = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .and_then(|sc| sc.namespace_options.as_ref());
        let pod_host_network = pod_ns.is_some_and(|n| n.network() as i32 == node_mode);
        let pod_host_pid = pod_ns.is_some_and(|n| n.pid() as i32 == node_mode);
        let pod_host_ipc = pod_ns.is_some_and(|n| n.ipc() as i32 == node_mode);

        let dns = config.dns_config.as_ref();
        let pod_id = self.pod_manager.reserve_pod(crate::core::state::PodState {
            attempt: metadata.attempt,
            namespace: metadata.namespace,
            name: metadata.name,
            uid: metadata.uid,
            runtime_handler: handler.clone(),
            labels: config.labels,
            annotations: config.annotations,
            log_directory: config.log_directory,
            hostname: config.hostname,
            dns_servers: dns.map(|d| d.servers.clone()).unwrap_or_default(),
            dns_searches: dns.map(|d| d.searches.clone()).unwrap_or_default(),
            dns_options: dns.map(|d| d.options.clone()).unwrap_or_default(),
            privileged: config.linux.as_ref().and_then(|l| l.security_context.as_ref()).is_some_and(|sc| sc.privileged),
            host_network: pod_host_network,
            host_pid: pod_host_pid,
            host_ipc: pod_host_ipc,
            cgroup_parent: config.linux.as_ref().map(|l| l.cgroup_parent.clone()).unwrap_or_default(),
            ..Default::default()
        }).await.map_err(|e| Status::internal(format!("Failed to create pod: {}", e)))?;

        let _guard = self.pod_manager.operation_guard(&pod_id).await;
        // A concurrent removal can observe the initial Pending record first.
        if self.pod_manager.get_pod_state(&pod_id).await.map_err(Status::from)?.is_none() {
            return Err(Status::aborted("sandbox removed during setup"));
        }
        if let Some(resources) = vm_resources {
            let id = pod_id.clone();
            let manager = self.pod_manager.clone();
            let dns = config.dns_config;
            let health = self.vm_ready.clone();
            let (reply, receive) = tokio::sync::oneshot::channel();
            // A canceled RPC must not abandon an in-flight CNI plugin. Keep
            // the guard through completion and unwind an unclaimed result.
            tokio::spawn(async move {
                let _guard = _guard;
                let result = manager.create_vm_network_locked(&id, metadata.attempt, dns.as_ref(), &resources).await;
                if result.is_ok() && !reply.is_closed() {
                    if reply.send(Ok(())).is_ok() { return; }
                    if let Err(e) = manager.rollback_vm_locked(&id).await {
                        tracing::warn!(pod_id=%id, %e, "unclaimed VM retained for cleanup");
                    }
                    return;
                }
                // A Pod may request more resources than this node can supply.
                // Recheck with the fixed probe before withdrawing the handler.
                if result.is_err() && health.ready.load(std::sync::atomic::Ordering::Acquire) {
                    health.recheck.notify_one();
                }
                let cleanup = manager.rollback_vm_locked(&id).await;
                let error = match (result, cleanup) {
                    (Err(e), Ok(())) => Status::from(e),
                    (Err(e), Err(other)) => Status::internal(format!("{e}; sandbox {id} cleanup failed: {other}")),
                    (Ok(()), Err(e)) => Status::from(e),
                    (Ok(()), Ok(())) => Status::cancelled("VM request canceled"),
                };
                let _ = reply.send(Err(error));
            });
            receive.await.map_err(|_| Status::internal("VM setup task stopped"))??;
            return Ok(Response::new(RunPodSandboxResponse { pod_sandbox_id: pod_id }));
        }
        let mut ipc_pid = None;
        let mut pause_pid = None;
        let mut network = None;
        let setup: Result<(), Status> = async {
            if handler == "runc" {
                // Route sysctls by namespace scope: net.* go to the netns holder,
                // IPC-scoped ones to a dedicated pod IPC namespace, the rest are
                // applied host-wide (best effort) before anything is created.
                let mut net_scoped = Vec::new();
                let mut ipc_scoped = Vec::new();
                for (k, v) in sysctls {
                    match crate::network::sysctl::scope_of(&k) {
                        crate::network::sysctl::Scope::Net => net_scoped.push((k, v)),
                        crate::network::sysctl::Scope::Ipc => ipc_scoped.push((k, v)),
                        crate::network::sysctl::Scope::Other => {
                            use std::path::Path;
                            let rel = k.replace('.', "/");
                            if let Err(e) =
                                std::fs::write(Path::new("/proc/sys").join(rel), format!("{v}\n"))
                            {
                                tracing::warn!(key=%k, error=%e, "host-scope sysctl apply failed");
                            }
                        }
                    }
                }
                if !ipc_scoped.is_empty() {
                    ipc_pid = Some(crate::network::podnet::create_pod_ipcns(&pod_id, &ipc_scoped)
                        .await.map_err(Status::from)?);
                    self.pod_manager.set_pod_ipc(&pod_id, ipc_pid).await.map_err(Status::from)?;
                }
                if !pod_host_pid {
                    pause_pid = Some(crate::network::podnet::create_pod_pidns(&pod_id)
                        .await.map_err(Status::from)?);
                    self.pod_manager.set_pod_pidns(&pod_id, pause_pid).await.map_err(Status::from)?;
                }
                if !pod_host_network {
                    network = self.pod_net.reserve(&pod_id, &net_scoped).await.map_err(Status::from)?;
                }
                if let Some(info) = network.as_mut() {
                    self.pod_manager.set_pod_network(&pod_id, &info.netns_path, "", info.holder_pid)
                        .await.map_err(Status::from)?;
                    self.pod_net.attach(&pod_id, info).await.map_err(Status::from)?;
                    self.pod_manager.set_pod_network(&pod_id, &info.netns_path, &info.ip, info.holder_pid)
                        .await.map_err(Status::from)?;
                    // Sandbox-level port publishing (CRI path): kubelet/critest
                    // declare mappings in the sandbox config, keyed to the pod.
                    let pm: Vec<crate::network::publish::PortMapping> = config
                        .port_mappings
                        .iter()
                        .filter(|m| m.host_port > 0 && m.container_port > 0)
                        .map(|m| crate::network::publish::PortMapping {
                            host_ip: m.host_ip.clone(),
                            host_port: m.host_port as u16,
                            ctr_port: m.container_port as u16,
                            proto: match crate::proto::runtime::v1::Protocol::try_from(m.protocol()) {
                                Ok(crate::proto::runtime::v1::Protocol::Udp) => "udp".to_string(),
                                _ => "tcp".to_string(),
                            },
                        })
                        .collect();
                    if !pm.is_empty() {
                        crate::network::publish::apply(&info.ip, &pod_id, &pm).await
                            .map_err(Status::from)?;
                    }
                }
            }
            self.pod_manager.start_pod_locked(&pod_id).await.map_err(Status::from)
        }.await;
        if let Err(error) = setup {
            self.release_pod_resources(&pod_id, network.as_ref(), pause_pid, ipc_pid).await.map_err(|cleanup| {
                Status::internal(format!("{error}; sandbox {pod_id} retained for cleanup: {cleanup}"))
            })?;
            self.pod_manager.remove_pod_locked(&pod_id).await.map_err(|cleanup| {
                Status::internal(format!("{error}; failed to remove incomplete sandbox {pod_id}: {cleanup}"))
            })?;
            return Err(error);
        }

        Ok(Response::new(RunPodSandboxResponse {
            pod_sandbox_id: pod_id,
        }))
    }

    async fn stop_pod_sandbox(
        &self,
        request: Request<StopPodSandboxRequest>,
    ) -> Result<Response<StopPodSandboxResponse>, Status> {
        let req = request.into_inner();
        let _guard = self.pod_manager.operation_guard(&req.pod_sandbox_id).await;

        self.stop_sandbox(&req.pod_sandbox_id).await?;

        Ok(Response::new(StopPodSandboxResponse {}))
    }

    async fn remove_pod_sandbox(
        &self,
        request: Request<RemovePodSandboxRequest>,
    ) -> Result<Response<RemovePodSandboxResponse>, Status> {
        let only_implicit_empty = request.metadata().contains_key("nautilus-if-implicit-empty");
        let id = request.into_inner().pod_sandbox_id;
        let _guard = self.pod_manager.operation_guard(&id).await;
        if let Some(pod) = self.pod_manager.get_pod_state(&id).await.map_err(Status::from)? {
            if only_implicit_empty && (
                pod.namespace != "ctl"
                || pod.labels.get("nautilus.ctl").map(String::as_str) != Some("true")
                || !self.container_manager.list_containers_in_pod(&id).await.map_err(Status::from)?.is_empty()
            ) {
                return Ok(Response::new(RemovePodSandboxResponse {}));
            }
            self.stop_sandbox(&id).await?;
            self.pod_manager.remove_pod_locked(&id).await.map_err(Status::from)?;
        }
        Ok(Response::new(RemovePodSandboxResponse {}))
    }

    async fn pod_sandbox_status(
        &self,
        request: Request<PodSandboxStatusRequest>,
    ) -> Result<Response<PodSandboxStatusResponse>, Status> {
        let req = request.into_inner();
        let pod = self.pod_manager.get_pod_state(&req.pod_sandbox_id).await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("pod sandbox {} not found", req.pod_sandbox_id)))?;

        let mut state = sandbox_state(&pod.status);
        let mut info = HashMap::new();
        if pod.runtime_handler == "vm" && (req.verbose || (pod.vm_attempt.is_some() && pod.status == PodStatus::Running)) {
            let health = self.pod_manager.observe_vm(&pod).await;
            if !matches!(health, Ok(Some(_))) { state = PodSandboxState::SandboxNotready; }
            let details = match health {
                Ok(Some(id)) => serde_json::json!({ "guestReachable": true, "bootId": id }),
                Ok(None) => serde_json::json!({ "guestReachable": false }),
                Err(error) => serde_json::json!({ "guestReachable": null, "error": error.to_string() }),
            };
            if req.verbose { info.insert("nautilus.vm".into(), details.to_string()); }
        }

        Ok(Response::new(PodSandboxStatusResponse {
            status: Some(runtime::v1::PodSandboxStatus {
                id: pod.id.clone(),
                metadata: Some(runtime::v1::PodSandboxMetadata {
                    name: pod.name,
                    uid: pod.uid,
                    namespace: pod.namespace,
                    attempt: pod.vm_attempt.as_ref().map_or(pod.attempt, |a| a.attempt),
                }),
                state: state as i32,
                created_at: pod.created_at.timestamp_nanos_opt().unwrap_or(0),
                network: (!pod.network_ip.is_empty()).then(|| {
                    runtime::v1::PodSandboxNetworkStatus {
                        ip: pod.network_ip.clone(),
                        additional_ips: Vec::new(),
                    }
                }),
                linux: pod.vm_attempt.as_ref().map(|_| runtime::v1::LinuxPodSandboxStatus {
                    namespaces: Some(runtime::v1::Namespace { options: Some(runtime::v1::NamespaceOption {
                        pid: runtime::v1::NamespaceMode::Container as i32, ..Default::default()
                    }) }),
                }),
                labels: pod.labels,
                annotations: pod.annotations,
                runtime_handler: pod.runtime_handler,
            }),
            info,
            containers_statuses: Vec::new(),
            timestamp: 0,
        }))
    }

    async fn list_pod_sandbox(
        &self,
        request: Request<ListPodSandboxRequest>,
    ) -> Result<Response<ListPodSandboxResponse>, Status> {
        let req = request.into_inner();
        let pods = self.pod_manager.list_pods().await
            .map_err(|e| Status::internal(format!("Failed to list pods: {}", e)))?;

        let filter = req.filter.as_ref();
        let candidates = pods.into_iter().filter(|p| filter.is_none_or(|f| {
            (f.id.is_empty() || f.id == p.id) &&
                f.label_selector.iter().all(|(k, v)| p.labels.get(k) == Some(v))
        }));
        let items = futures::stream::iter(candidates).map(|mut p| async move {
            if p.vm_attempt.is_some() && p.status == PodStatus::Running &&
                !matches!(self.pod_manager.observe_vm(&p).await, Ok(Some(_))) {
                p.status = PodStatus::Unknown;
            }
            if !matches_sandbox_state(filter.and_then(|f| f.state.as_ref()), &p.status) {
                return None;
            }
            Some(runtime::v1::PodSandbox {
                id: p.id,
                metadata: Some(PodSandboxMetadata { name: p.name, uid: p.uid, namespace: p.namespace,
                    attempt: p.vm_attempt.as_ref().map_or(p.attempt, |a| a.attempt) }),
                state: sandbox_state(&p.status) as i32,
                created_at: p.created_at.timestamp_nanos_opt().unwrap_or(0),
                labels: p.labels, annotations: p.annotations, runtime_handler: p.runtime_handler,
            })
        }).buffered(8).filter_map(futures::future::ready).collect().await;

        Ok(Response::new(ListPodSandboxResponse { items }))
    }

    async fn create_container(
        &self,
        request: Request<CreateContainerRequest>,
    ) -> Result<Response<CreateContainerResponse>, Status> {
        let req = request.into_inner();
        let _guard = self.pod_manager.operation_guard(&req.pod_sandbox_id).await;
        let config = req.config.ok_or_else(|| {
            Status::invalid_argument("Container config is required")
        })?;
        
        let metadata = config.metadata.as_ref().ok_or_else(|| {
            Status::invalid_argument("Container metadata is required")
        })?;
        
        let image = config.image.as_ref().ok_or_else(|| {
            Status::invalid_argument("Container image is required")
        })?;

        // P1b: in-process policy gate before any resource work.
        self.policy
            .enforce(&crate::policy::PolicyRequest {
                hook: "create_container",
                pod_id: &req.pod_sandbox_id,
                container_id: "",
                runtime_handler: "",
                image: &image.image,
                labels: &config.labels,
                annotations: &config.annotations,
                args: &config.args,
            })
            .await
            .map_err(Status::from)?;

        // Inherit the sandbox backend from the pod this container joins.
        let pod = self.pod_manager.get_pod_state(&req.pod_sandbox_id).await
            .map_err(Status::from)?
            .ok_or_else(|| {
                Status::not_found(format!(
                    "pod sandbox {} not found",
                    req.pod_sandbox_id
                ))
            })?;

        if pod.status != PodStatus::Running {
            return Err(Status::failed_precondition("pod sandbox is not ready"));
        }

        if pod.runtime_handler == "vm" {
            validate_vm_container(&config).map_err(|(code,msg)| Status::new(code,msg))?;
        }
        let strict_groups = config.linux.as_ref().and_then(|l| l.security_context.as_ref())
            .is_some_and(|s| s.supplemental_groups_policy == SupplementalGroupsPolicy::Strict as i32);

        // CRI resource constraints → backend-agnostic limits.
        let resources = config
            .linux
            .as_ref()
            .and_then(|l| l.resources.as_ref())
            .map(|r| crate::sandbox::ResourceLimits {
                cpu_period: r.cpu_period,
                cpu_quota: r.cpu_quota,
                cpu_shares: r.cpu_shares,
                cpuset_cpus: r.cpuset_cpus.clone(),
                cpuset_mems: r.cpuset_mems.clone(),
                memory_limit_in_bytes: r.memory_limit_in_bytes,
                oom_score_adj: r.oom_score_adj as i32,
                guest_unified: if pod.runtime_handler == "vm" { r.unified.clone() } else { Default::default() },
                guest_no_hugepages: if pod.runtime_handler == "vm" { r.hugepage_limits.iter().map(|p| p.page_size.clone()).collect() } else { Vec::new() },
            });

        // CRI security context → per-container overrides.
        tracing::info!(
            has_linux = config.linux.is_some(),
            has_secctx = config.linux.as_ref().and_then(|l| l.security_context.as_ref()).is_some(),
            runas_uid = config.linux.as_ref().and_then(|l| l.security_context.as_ref()).and_then(|sc| sc.run_as_user.as_ref()).map(|v| v.value).unwrap_or(-1),
            apparmor_ptype = config.linux.as_ref().and_then(|l| l.security_context.as_ref()).and_then(|sc| sc.apparmor.as_ref().map(|a| a.profile_type)),
            apparmor_ref = config.linux.as_ref().and_then(|l| l.security_context.as_ref()).and_then(|sc| sc.apparmor.as_ref().map(|a| a.localhost_ref.clone())),
            "container security probe"
        );
        let security = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .map(|sc| crate::sandbox::ContainerSecurity {
                no_new_privs: sc.no_new_privs,
                readonly_rootfs: sc.readonly_rootfs,
                seccomp_profile_path: {
                    use crate::proto::runtime::v1::security_profile::ProfileType;
                    // New SecurityProfile field wins; the deprecated
                    // seccomp_profile_path string is the fallback.
                    let from_struct = match sc.seccomp.as_ref() {
                        Some(p) if p.profile_type == ProfileType::Localhost as i32 => {
                            p.localhost_ref.trim_end_matches('/').to_string()
                        }
                        Some(p) if p.profile_type == ProfileType::Unconfined as i32 => {
                            "unconfined".to_string()
                        }
                        Some(_) => "runtime/default".to_string(), // RuntimeDefault or empty
                        None => String::new(),
                    };
                    #[allow(deprecated)] // intentional fallback for old clients
                    let from_deprecated = sc.seccomp_profile_path.clone();
                    if !from_struct.is_empty() {
                        from_struct
                    } else if from_deprecated.is_empty() {
                        String::new()
                    } else {
                        from_deprecated
                    }
                },
                apparmor_profile: {
                    use crate::proto::runtime::v1::security_profile::ProfileType;
                    // New-style SecurityProfile field wins; deprecated
                    // `apparmor_profile` string is the fallback and uses the
                    // same `localhost/<name>` or `runtime/default` refs.
                    let from_struct = match sc.apparmor.as_ref() {
                        Some(a) if a.profile_type == ProfileType::Localhost as i32 => a
                            .localhost_ref
                            .trim_start_matches("localhost/")
                            .trim_end_matches('/')
                            .to_string(),
                        Some(a) if a.profile_type == ProfileType::RuntimeDefault as i32 => {
                            "runtime/default".to_string()
                        }
                        _ => String::new(),
                    };
                    #[allow(deprecated)] // intentional fallback for old clients
                    let from_deprecated = sc.apparmor_profile.clone();
                    if !from_struct.is_empty() {
                        from_struct
                    } else if let Some(name) = from_deprecated.strip_prefix("localhost/") {
                        name.trim_end_matches('/').to_string()
                    } else if from_deprecated == "runtime/default" {
                        "runtime/default".to_string()
                    } else {
                        from_deprecated
                    }
                },
                run_as_uid: sc.run_as_user.as_ref().map(|v| v.value as u32),
                run_as_gid: sc.run_as_group.as_ref().map(|v| v.value as u32),
                run_as_name: sc.run_as_username.clone(),
                supplemental_groups: sc.supplemental_groups.iter().map(|g| *g as u32).collect(),
                add_capabilities: sc.capabilities.as_ref().map(|c| c.add_capabilities.clone()).unwrap_or_default(),
                drop_capabilities: sc.capabilities.as_ref().map(|c| c.drop_capabilities.clone()).unwrap_or_default(),
                privileged: sc.privileged,
                masked_paths: sc.masked_paths.clone(),
                readonly_paths: sc.readonly_paths.clone(),
                host_network: sc
                    .namespace_options
                    .as_ref()
                    .map(|n| n.network()
                        == crate::proto::runtime::v1::NamespaceMode::Node)
                    .unwrap_or(false),
                host_pid: sc
                    .namespace_options
                    .as_ref()
                    .map(|n| n.pid()
                        == crate::proto::runtime::v1::NamespaceMode::Node)
                    .unwrap_or(false),
                host_ipc: sc
                    .namespace_options
                    .as_ref()
                    .map(|n| n.ipc()
                        == crate::proto::runtime::v1::NamespaceMode::Node)
                    .unwrap_or(false),
                share_pod_pid: false, // resolved after the parse (mode defaults)
            });

        // Privileged sandboxes give their containers full caps even when the
        // container itself does not request privileged (critest mount
        // propagation tests run in a privileged pod).
        let pod_priv = pod.privileged;
        let mut security = security;
        if pod_priv {
            // A container without its own security context still inherits
            // full capabilities from a privileged sandbox.
            security = Some(match security {
                Some(mut sec) => {
                    sec.privileged = true;
                    sec
                }
                None => crate::sandbox::ContainerSecurity {
                    privileged: true,
                    ..Default::default()
                },
            });
        }

        // CRI validation: run_as_group without run_as_user must error.
        if let Some(sc) = config.linux.as_ref().and_then(|l| l.security_context.as_ref()) {
            if sc.run_as_user.is_some() && !sc.run_as_username.is_empty() {
                return Err(Status::invalid_argument("run_as_user and run_as_username are mutually exclusive"));
            }
            if sc.run_as_group.is_some() && sc.run_as_user.is_none() && sc.run_as_username.is_empty() {
                return Err(Status::invalid_argument(
                    "run_as_group must not be specified without run_as_user or run_as_username",
                ));
            }
        }

        // CRI pid namespace mode: NODE = host, CONTAINER = fresh per
        // container, POD/absent = join the pod's shared PID namespace.
        let pid_node = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .and_then(|sc| sc.namespace_options.as_ref())
            .is_some_and(|n| n.pid() == crate::proto::runtime::v1::NamespaceMode::Node);
        let pid_container = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .and_then(|sc| sc.namespace_options.as_ref())
            .is_some_and(|n| n.pid() == crate::proto::runtime::v1::NamespaceMode::Container);
        let share_pod_pid = !pid_node && !pid_container;

        // hostNetwork / pid / ipc NODE modes are pod-scoped: containers of
        // such sandboxes stay on the host namespaces even when their own
        // config is silent.
        tracing::debug!(
            pod_host_network = pod.host_network,
            pod_host_pid = pod.host_pid,
            pod_host_ipc = pod.host_ipc,
            "pod namespace-mode inheritance probe"
        );
        if pod.host_network || pod.host_pid || pod.host_ipc {
            let share_pod_pid = security.as_ref().is_some_and(|s| s.share_pod_pid);
            security = Some(match security {
                Some(mut sec) => {
                    sec.host_network |= pod.host_network;
                    sec.host_pid |= pod.host_pid;
                    sec.host_ipc |= pod.host_ipc;
                    sec
                }
                None => crate::sandbox::ContainerSecurity {
                    host_network: pod.host_network,
                    host_pid: pod.host_pid,
                    host_ipc: pod.host_ipc,
                    share_pod_pid,
                    ..Default::default()
                },
            });
        }
        // Record the resolved pid mode on the security context (default true
        // for containers without a security context: pid=POD).
        match security.as_mut() {
            Some(sec) => sec.share_pod_pid = share_pod_pid,
            None => {
                security = Some(crate::sandbox::ContainerSecurity {
                    share_pod_pid,
                    ..Default::default()
                });
            }
        }

        // CRI envs → key/value map; backends merge them over image config env.
        // (The vendored proto types value as bytes; treat it as UTF-8.)
        let env: HashMap<String, String> = config
            .envs
            .into_iter()
            .map(|kv| (kv.key, String::from_utf8_lossy(&kv.value).to_string()))
            .collect();

        // AppArmor parity: a named profile that is not loaded on the node
        // must fail container creation (critest expects an error), instead of
        // silently starting unconfined.
        if let Some(sec) = security.as_ref() {
            if !sec.apparmor_profile.is_empty()
                && !crate::sandbox::spec::apparmor_profile_loaded(&sec.apparmor_profile)
            {
                return Err(Status::invalid_argument(format!(
                    "apparmor profile '{}' is not loaded",
                    sec.apparmor_profile
                )));
            }
        }

        let image_ref = self.images.workload_reference(&image.image).await.map_err(Status::from)?;
        let container_id = self.container_manager.create_with_process_options(crate::runtime::container::CreateWorkload {
            pod_id:&req.pod_sandbox_id, name:&metadata.name, image:&image_ref, sandbox_kind:&pod.runtime_handler,
            command: config.command, args: config.args, log_path: &config.log_path,
            volumes: config
                .mounts
                .iter()
                .map(|m| crate::sandbox::VolumeMount {
                    host_path: std::path::PathBuf::from(m.host_path.as_str()),
                    container_path: m.container_path.clone(),
                    readonly: m.readonly,
                    propagation: cri_propagation_to_oci(m.propagation),
                })
                .collect(),
            env, resources, security, labels:config.labels, annotations:config.annotations, stdin:config.stdin,
            working_dir:&config.working_dir, strict_groups, tty:config.tty, stdin_once:config.stdin_once, attempt:metadata.attempt,
        }).await.map_err(Status::from)?;
        
        if let Err(error) = self.pod_manager.add_container_to_pod(&req.pod_sandbox_id, &container_id).await {
            self.container_manager.remove_container(&container_id).await.map_err(Status::from)?;
            return Err(Status::from(error));
        }

        Ok(Response::new(CreateContainerResponse {
            container_id,
        }))
    }

    async fn start_container(
        &self,
        request: Request<StartContainerRequest>,
    ) -> Result<Response<StartContainerResponse>, Status> {
        let req = request.into_inner();

        let before = self.container_manager.get_container_state(&req.container_id).await.map_err(Status::from)?
            .ok_or_else(|| Status::not_found("container not found"))?;
        let _guard = self.pod_manager.operation_guard(&before.pod_id).await;
        if !self.pod_manager.get_pod_state(&before.pod_id).await.map_err(Status::from)?
            .is_some_and(|pod| pod.status == PodStatus::Running) {
            return Err(Status::failed_precondition("pod sandbox is not ready"));
        }
        if let Some(c) = self.container_manager.get_container_state(&req.container_id).await.map_err(Status::from)? {
            self.policy
                .enforce(&crate::policy::PolicyRequest {
                    hook: "start_container",
                    pod_id: &c.pod_id,
                    container_id: &c.id,
                    runtime_handler: &c.sandbox_kind,
                    image: &c.image,
                    labels: &c.labels,
                    annotations: &c.annotations,
                    args: &[],
                })
                .await
                .map_err(Status::from)?;
        }

        self.container_manager.start_container(&req.container_id).await
            .map_err(|e| Status::internal(format!("Failed to start container: {}", e)))?;
        
        Ok(Response::new(StartContainerResponse {}))
    }

    async fn stop_container(
        &self,
        request: Request<StopContainerRequest>,
    ) -> Result<Response<StopContainerResponse>, Status> {
        let req = request.into_inner();
        let timeout = if req.timeout > 0 { Some(req.timeout as u64) } else { None };

        if let Some(before) = self.container_manager.get_container_state(&req.container_id).await.map_err(Status::from)? {
            let _guard = self.pod_manager.operation_guard(&before.pod_id).await;
            if self.container_manager.get_container_state(&req.container_id).await.map_err(Status::from)?
                .is_some_and(|c| c.status == ContainerStatus::Running || c.vm.as_ref().is_some_and(|v| !v.preparing && !v.logs_complete)) {
                self.container_manager.stop_container(&req.container_id, timeout).await.map_err(Status::from)?;
            }
        }

        Ok(Response::new(StopContainerResponse {}))
    }

    async fn remove_container(
        &self,
        request: Request<RemoveContainerRequest>,
    ) -> Result<Response<RemoveContainerResponse>, Status> {
        let condition = match request.metadata().get("nautilus-remove-if").map(|v| v.as_encoded_bytes()) {
            None => None,
            Some(b"exited") => Some("exited"),
            Some(b"not-running") => Some("not-running"),
            Some(_) => return Err(Status::invalid_argument("unknown container removal condition")),
        };
        let req = request.into_inner();
        if let Some(before) = self.container_manager.get_container_state(&req.container_id).await.map_err(Status::from)? {
            let _guard = self.pod_manager.operation_guard(&before.pod_id).await;
            if let Some(current) = self.container_manager.get_container_state(&req.container_id).await.map_err(Status::from)? {
                let permitted = match condition {
                    Some("exited") => current.status == ContainerStatus::Stopped,
                    Some("not-running") => !matches!(current.status, ContainerStatus::Running | ContainerStatus::Paused),
                    _ => true,
                };
                if !permitted {
                    return Err(Status::failed_precondition(format!("container {} is {:?}", current.id, current.status)));
                }
                self.container_manager.remove_container(&req.container_id).await.map_err(Status::from)?;
                if self.pod_manager.get_pod_state(&before.pod_id).await.map_err(Status::from)?.is_some() {
                    self.pod_manager.remove_container_from_pod(&before.pod_id, &req.container_id).await.map_err(Status::from)?;
                }
            }
        }

        Ok(Response::new(RemoveContainerResponse {}))
    }

    async fn list_containers(
        &self,
        request: Request<ListContainersRequest>,
    ) -> Result<Response<ListContainersResponse>, Status> {
        let req = request.into_inner();
        
        let containers = self.container_manager.observed_containers().await
            .map_err(|e| Status::internal(format!("Failed to list containers: {}", e)))?;
        
        let mut filtered_containers = Vec::new();
        
        for container in containers {
            // Apply filters if provided
            if let Some(filter) = &req.filter {
                if !filter.id.is_empty() && container.id != filter.id {
                    continue;
                }
                
                if !filter.pod_sandbox_id.is_empty() && container.pod_id != filter.pod_sandbox_id {
                    continue;
                }
                
                if let Some(state) = &filter.state {
                    let container_state = match container.status {
                        ContainerStatus::Created => runtime::v1::ContainerState::ContainerCreated,
                        ContainerStatus::Running => runtime::v1::ContainerState::ContainerRunning,
                        ContainerStatus::Stopped => runtime::v1::ContainerState::ContainerExited,
                        ContainerStatus::Paused => runtime::v1::ContainerState::ContainerRunning,
                        ContainerStatus::Unknown => runtime::v1::ContainerState::ContainerUnknown,
                    };
                    
                    if state.state != container_state as i32 {
                        continue;
                    }
                }
            }
            
            let container_status = match container.status {
                ContainerStatus::Created => runtime::v1::ContainerState::ContainerCreated,
                ContainerStatus::Running => runtime::v1::ContainerState::ContainerRunning,
                ContainerStatus::Stopped => runtime::v1::ContainerState::ContainerExited,
                ContainerStatus::Paused => runtime::v1::ContainerState::ContainerRunning,
                ContainerStatus::Unknown => runtime::v1::ContainerState::ContainerUnknown,
            };
            
            let image_id = self.images.container_image_id(&container).await;
            filtered_containers.push(Container {
                id: container.id,
                pod_sandbox_id: container.pod_id,
                metadata: Some(ContainerMetadata {
                    name: container.name,
                    attempt: container.attempt,
                }),
                image: Some(ImageSpec {
                    image: container.image.clone(),
                    annotations: HashMap::new(),
                    runtime_handler: String::new(),
                    user_specified_image: String::new(),
                    image_ref: String::new(),
                }),
                image_id,
                image_ref: container.image.clone(),
                state: container_status as i32,
                created_at: container.created_at.timestamp_nanos_opt().unwrap_or(0),
                labels: container.labels,
                annotations: container.annotations,
            });
        }
        
        Ok(Response::new(ListContainersResponse {
            containers: filtered_containers,
        }))
    }

    async fn container_status(
        &self,
        request: Request<ContainerStatusRequest>,
    ) -> Result<Response<ContainerStatusResponse>, Status> {
        let req = request.into_inner();
        let c = self.container_manager.observed_container(&req.container_id).await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("container {} not found", req.container_id)))?;

        let state = match c.status {
            ContainerStatus::Created => runtime::v1::ContainerState::ContainerCreated,
            ContainerStatus::Running | ContainerStatus::Paused => {
                runtime::v1::ContainerState::ContainerRunning
            }
            ContainerStatus::Stopped => runtime::v1::ContainerState::ContainerExited,
            ContainerStatus::Unknown => runtime::v1::ContainerState::ContainerUnknown,
        };

        let image_id = self.images.container_image_id(&c).await;
        Ok(Response::new(ContainerStatusResponse {
            status: Some(runtime::v1::ContainerStatus {
                id: c.id.clone(),
                metadata: Some(ContainerMetadata {
                    name: c.name,
                    attempt: c.attempt,
                }),
                state: state as i32,
                created_at: c.created_at.timestamp_nanos_opt().unwrap_or(0),
                started_at: c.started_at.and_then(|t| t.timestamp_nanos_opt()).unwrap_or(0),
                finished_at: c.finished_at.and_then(|t| t.timestamp_nanos_opt()).unwrap_or(0),
                exit_code: c.exit_code.unwrap_or(0),
                image: Some(ImageSpec {
                    image: c.image.clone(),
                    annotations: HashMap::new(),
                    runtime_handler: c.sandbox_kind.clone(),
                    user_specified_image: String::new(),
                    image_ref: c.image.clone(),
                }),
                image_ref: c.image.clone(),
                reason: c.finish_reason.clone(),
                message: String::new(),
                labels: c.labels.clone(),
                annotations: c.annotations,
                mounts: c
                    .volumes
                    .iter()
                    .map(|v| runtime::v1::Mount {
                        container_path: v.container_path.clone(),
                        host_path: v.host_path.to_string_lossy().into_owned(),
                        readonly: v.readonly,
                        selinux_relabel: false,
                        propagation: oci_propagation_to_cri(&v.propagation),
                        uid_mappings: Vec::new(),
                        gid_mappings: Vec::new(),
                        recursive_read_only: false,
                        image: None,
                        image_sub_path: String::new(),
                        mount_options: Vec::new(),
                    })
                    .collect(),
                log_path: c.log_path.unwrap_or_default(),
                resources: None,
                image_id,
                user: None,
                stop_signal: 0, // SIGTERM (proto enum 0); not configurable yet
            }),
            info: HashMap::new(),
        }))
    }

    async fn update_container_resources(
        &self,
        _request: Request<UpdateContainerResourcesRequest>,
    ) -> Result<Response<UpdateContainerResourcesResponse>, Status> {
        Err(Status::unimplemented("not implemented yet"))
    }

    async fn reopen_container_log(
        &self,
        request: Request<ReopenContainerLogRequest>,
    ) -> Result<Response<ReopenContainerLogResponse>, Status> {
        let req = request.into_inner();
        // After a rotate (unlink) the runtime must recreate the log file even
        // before the next line arrives. Recreate the container's host log
        // destination directly.
        if let Some(c) = self
            .container_manager
            .get_container_state(&req.container_id)
            .await
            .map_err(Status::from)?
        {
            if c.sandbox_kind == "vm" {
                self.container_manager.reopen_guest_log(&c.id).await.map_err(Status::from)?;
                return Ok(Response::new(ReopenContainerLogResponse {}));
            }
            if let Some(lp) = c.log_path {
                if let Some(parent) = std::path::Path::new(&lp).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::File::create(&lp);
            }
        }
        Ok(Response::new(ReopenContainerLogResponse {}))
    }

    async fn exec_sync(
        &self,
        request: Request<ExecSyncRequest>,
    ) -> Result<Response<ExecSyncResponse>, Status> {
        let req = request.into_inner();
        if req.cmd.is_empty() {
            return Err(Status::invalid_argument("exec args must not be empty"));
        }
        // The vendored proto passes argv directly on ExecSyncRequest.
        let env: Vec<(String, String)> = Vec::new();
        let cwd: Option<&str> = None;

        let timeout = if req.timeout > 0 {
            Some(std::time::Duration::from_secs(req.timeout as u64))
        } else {
            None
        };
        let (code, stdout, stderr) = self
            .container_manager
            .exec_sync(&req.container_id, &req.cmd, &env, cwd, timeout)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(ExecSyncResponse {
            exit_code: code,
            // 16MB cap per CVE-2022-1708/31030.
            stdout: stdout.into_iter().take(16 * 1024 * 1024).collect(),
            stderr: stderr.into_iter().take(16 * 1024 * 1024).collect(),
        }))
    }

    async fn exec(&self, request: Request<ExecRequest>) -> Result<Response<ExecResponse>, Status> {
        let req = request.into_inner();
        if req.cmd.is_empty() {
            return Err(Status::invalid_argument("exec args must not be empty"));
        }
        let c = self
            .container_manager
            .get_container_state(&req.container_id)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| {
                Status::not_found(format!("container {} not found", req.container_id))
            })?;
        // A short batch workload can finish between start and exec; delegate
        // the liveness judgement to the backend (same tolerance as execSync).
        if !matches!(
            c.status,
            ContainerStatus::Running | ContainerStatus::Stopped
        ) {
            return Err(Status::invalid_argument(format!(
                "container {} is not running, current status: {:?}",
                req.container_id, c.status
            )));
        }
        if c.sandbox_kind == "vm" && (req.tty && req.stderr || !(req.stdin || req.stdout || req.stderr)) {
            return Err(Status::invalid_argument("invalid VM exec stream selection"));
        }
        let url = crate::cri::streaming::exec_url(
            self.container_manager.clone(),
            self._state_manager.clone(),
            req.container_id,
            req.cmd,
            req.tty,
            req.stdin,
            req.stdout,
            req.stderr,
        )
        .await
        .map_err(Status::from)?;
        Ok(Response::new(ExecResponse { url }))
    }

    async fn attach(
        &self,
        request: Request<AttachRequest>,
    ) -> Result<Response<AttachResponse>, Status> {
        let req = request.into_inner();
        let c = self
            .container_manager
            .get_container_state(&req.container_id)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| {
                Status::not_found(format!("container {} not found", req.container_id))
            })?;
        if !matches!(
            c.status,
            ContainerStatus::Running | ContainerStatus::Stopped
        ) {
            return Err(Status::invalid_argument(format!(
                "container {} is not running, current status: {:?}",
                req.container_id, c.status
            )));
        }
        if c.sandbox_kind == "vm" && (req.tty && req.stderr || !(req.stdin || req.stdout || req.stderr)) {
            return Err(Status::invalid_argument("invalid VM attach stream selection"));
        }
        let url = crate::cri::streaming::attach_url(
            self.container_manager.clone(),
            self._state_manager.clone(),
            req.container_id,
            req.tty,
            req.stdin,
            req.stdout,
            req.stderr,
        )
        .await
        .map_err(Status::from)?;
        Ok(Response::new(AttachResponse { url }))
    }

    async fn port_forward(
        &self,
        request: Request<PortForwardRequest>,
    ) -> Result<Response<PortForwardResponse>, Status> {
        let req = request.into_inner();
        let pod = self
            ._state_manager
            .get_pod_state(&req.pod_sandbox_id)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| {
                Status::not_found(format!("pod sandbox {} not found", req.pod_sandbox_id))
            })?;
        if pod.status != PodStatus::Running {
            return Err(Status::invalid_argument(format!(
                "pod sandbox {} is not ready, current status: {:?}",
                req.pod_sandbox_id, pod.status
            )));
        }
        let ports: Vec<u16> = req.port.into_iter().map(|n| u16::try_from(n).ok().filter(|p| *p > 0)
            .ok_or_else(|| Status::invalid_argument("invalid forwarding port"))).collect::<Result<_,_>>()?;
        let url = crate::cri::streaming::port_forward_url(
            self.container_manager.clone(),
            self._state_manager.clone(),
            req.pod_sandbox_id,
            ports,
        )
        .await
        .map_err(Status::from)?;
        Ok(Response::new(PortForwardResponse { url }))
    }

    async fn container_stats(
        &self,
        request: Request<ContainerStatsRequest>,
    ) -> Result<Response<ContainerStatsResponse>, Status> {
        let req = request.into_inner();
        let stats = self.stats_response(&req.container_id).await?;
        Ok(Response::new(ContainerStatsResponse { stats: Some(stats) }))
    }

    async fn list_container_stats(
        &self,
        request: Request<ListContainerStatsRequest>,
    ) -> Result<Response<ListContainerStatsResponse>, Status> {
        let req = request.into_inner();
        let filter = req.filter.unwrap_or_default();

        let containers = self
            .container_manager
            .list_containers()
            .await
            .map_err(Status::from)?;

        let matches = containers.into_iter().filter(|c| {
            (filter.id.is_empty() || filter.id == c.id)
                && (filter.pod_sandbox_id.is_empty() || filter.pod_sandbox_id == c.pod_id)
                && filter.label_selector.iter().all(|(k, v)| c.labels.get(k) == Some(v))
        });
        let stats = futures::stream::iter(matches)
            .map(|c| async move { self.stats_response(&c.id).await.ok() })
            .buffered(8)
            .filter_map(std::future::ready)
            .collect().await;
        Ok(Response::new(ListContainerStatsResponse { stats }))
    }

    async fn update_runtime_config(
        &self,
        request: Request<UpdateRuntimeConfigRequest>,
    ) -> Result<Response<UpdateRuntimeConfigResponse>, Status> {
        if let Some(network) = request.into_inner().runtime_config.and_then(|config| config.network_config) {
            if !network.pod_cidr.is_empty() && !self.pod_net.matches_pod_cidr(&network.pod_cidr).map_err(Status::from)? {
                return Err(Status::failed_precondition("kubelet PodCIDR differs from the configured CNI allocation"));
            }
        }
        Ok(Response::new(UpdateRuntimeConfigResponse {}))
    }

    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        use crate::proto::runtime::v1 as rt;
        let cond = |t: &str| rt::RuntimeCondition {
            r#type: t.to_string(),
            status: true,
            reason: "".to_string(),
            message: "".to_string(),
        };
        let mut names = self.sandbox_registry.list_handlers();
        names.insert(0, String::new());
        if self.vm_ready.ready.load(std::sync::atomic::Ordering::Acquire) {
            names.push("vm".into());
        }
        let handlers = names.into_iter().map(|name| rt::RuntimeHandler {
            name,
            features: Some(rt::RuntimeHandlerFeatures::default()),
        }).collect();
        Ok(Response::new(StatusResponse {
            // kubelet/crictl require BOTH conditions to consider the runtime
            // sane.
            status: Some(rt::RuntimeStatus {
                conditions: vec![cond("RuntimeReady"), cond("NetworkReady")],
            }),
            runtime_handlers: handlers,
            features: None,
            info: Default::default(),
        }))
    }

    async fn checkpoint_container(
        &self,
        _request: Request<CheckpointContainerRequest>,
    ) -> Result<Response<CheckpointContainerResponse>, Status> {
        Err(Status::unimplemented("not implemented yet"))
    }

    async fn get_container_events(
        &self,
        _request: Request<GetEventsRequest>,
    ) -> Result<Response<Self::GetContainerEventsStream>, Status> {
        Err(Status::unimplemented("not implemented yet"))
    }

    async fn list_metric_descriptors(
        &self,
        _request: Request<ListMetricDescriptorsRequest>,
    ) -> Result<Response<ListMetricDescriptorsResponse>, Status> {
        Ok(Response::new(ListMetricDescriptorsResponse {
            descriptors: metrics::descriptors(),
        }))
    }

    async fn list_pod_sandbox_stats(
        &self,
        request: Request<ListPodSandboxStatsRequest>,
    ) -> Result<Response<ListPodSandboxStatsResponse>, Status> {
        let filter = request.into_inner().filter.unwrap_or_default();
        let pods = self.pod_manager.list_pods().await.map_err(Status::from)?;
        let matches = pods.into_iter().filter(|p| {
            (filter.id.is_empty() || filter.id == p.id)
                && filter.label_selector.iter().all(|(k, v)| p.labels.get(k) == Some(v))
        });
        let stats = futures::stream::iter(matches)
            .map(|p| async move { self.pod_stats_response(&p.id).await.ok() })
            .buffered(8)
            .filter_map(std::future::ready)
            .collect().await;
        Ok(Response::new(ListPodSandboxStatsResponse { stats }))
    }

    async fn pod_sandbox_stats(
        &self,
        request: Request<PodSandboxStatsRequest>,
    ) -> Result<Response<PodSandboxStatsResponse>, Status> {
        let req = request.into_inner();
        let stats = self.pod_stats_response(&req.pod_sandbox_id).await?;
        Ok(Response::new(PodSandboxStatsResponse {
            stats: Some(stats),
        }))
    }

    type GetContainerEventsStream =
        Pin<Box<dyn Stream<Item = Result<ContainerEventResponse, Status>> + Send + 'static>>;

    async fn list_pod_sandbox_metrics(
        &self,
        request: Request<ListPodSandboxMetricsRequest>,
    ) -> Result<Response<ListPodSandboxMetricsResponse>, Status> {
        let samples = self
            .list_pod_sandbox_stats(request.map(|_| ListPodSandboxStatsRequest { filter: None }))
            .await?
            .into_inner()
            .stats;
        Ok(Response::new(ListPodSandboxMetricsResponse {
            pod_metrics: samples.into_iter().filter_map(metrics::pod_metrics).collect(),
        }))
    }

    async fn runtime_config(
        &self,
        _request: Request<RuntimeConfigRequest>,
    ) -> Result<Response<RuntimeConfigResponse>, Status> {
        use crate::proto::runtime::v1 as rt;
        Ok(Response::new(RuntimeConfigResponse {
            linux: Some(rt::LinuxRuntimeConfiguration {
                cgroup_driver: if self._config.runtime.runc.systemd_cgroup { rt::CgroupDriver::Systemd } else { rt::CgroupDriver::Cgroupfs } as i32,
            }),
        }))
    }

    type StreamPodSandboxesStream = ListStream<StreamPodSandboxesResponse>;

    async fn stream_pod_sandboxes(
        &self,
        request: Request<StreamPodSandboxesRequest>,
    ) -> Result<Response<Self::StreamPodSandboxesStream>, Status> {
        let reply = self
            .list_pod_sandbox(request.map(|r| ListPodSandboxRequest { filter: r.filter }))
            .await?
            .into_inner();
        Ok(Response::new(batches(reply.items, |pod_sandboxes| {
            StreamPodSandboxesResponse { pod_sandboxes }
        })))
    }

    type StreamContainersStream = ListStream<StreamContainersResponse>;

    async fn stream_containers(
        &self,
        request: Request<StreamContainersRequest>,
    ) -> Result<Response<Self::StreamContainersStream>, Status> {
        let reply = self
            .list_containers(request.map(|r| ListContainersRequest { filter: r.filter }))
            .await?
            .into_inner();
        Ok(Response::new(batches(reply.containers, |containers| {
            StreamContainersResponse { containers }
        })))
    }

    type StreamContainerStatsStream = ListStream<StreamContainerStatsResponse>;

    async fn stream_container_stats(
        &self,
        request: Request<StreamContainerStatsRequest>,
    ) -> Result<Response<Self::StreamContainerStatsStream>, Status> {
        let reply = self
            .list_container_stats(request.map(|r| ListContainerStatsRequest { filter: r.filter }))
            .await?
            .into_inner();
        Ok(Response::new(batches(reply.stats, |container_stats| {
            StreamContainerStatsResponse { container_stats }
        })))
    }

    type StreamPodSandboxStatsStream = ListStream<StreamPodSandboxStatsResponse>;

    async fn stream_pod_sandbox_stats(
        &self,
        request: Request<StreamPodSandboxStatsRequest>,
    ) -> Result<Response<Self::StreamPodSandboxStatsStream>, Status> {
        let reply = self
            .list_pod_sandbox_stats(request.map(|r| ListPodSandboxStatsRequest { filter: r.filter }))
            .await?
            .into_inner();
        Ok(Response::new(batches(reply.stats, |pod_sandbox_stats| {
            StreamPodSandboxStatsResponse { pod_sandbox_stats }
        })))
    }

    type StreamPodSandboxMetricsStream = ListStream<StreamPodSandboxMetricsResponse>;

    async fn stream_pod_sandbox_metrics(
        &self,
        request: Request<StreamPodSandboxMetricsRequest>,
    ) -> Result<Response<Self::StreamPodSandboxMetricsStream>, Status> {
        let reply = self
            .list_pod_sandbox_metrics(request.map(|_| ListPodSandboxMetricsRequest {}))
            .await?
            .into_inner();
        Ok(Response::new(batches(reply.pod_metrics, |pod_sandbox_metrics| {
            StreamPodSandboxMetricsResponse { pod_sandbox_metrics }
        })))
    }

    async fn update_pod_sandbox_resources(
        &self,
        _request: Request<UpdatePodSandboxResourcesRequest>,
    ) -> Result<Response<UpdatePodSandboxResourcesResponse>, Status> {
        // Pod-level in-place resize; lands alongside P1a pod networking since it
        // needs pod-level cgroup ownership.
        Err(Status::unimplemented("pod-level resize not implemented yet"))
    }

    async fn checkpoint_pod(
        &self,
        _request: Request<CheckpointPodRequest>,
    ) -> Result<Response<CheckpointPodResponse>, Status> {
        Err(Status::unimplemented("pod checkpoint/restore is out of scope for now"))
    }

    async fn restore_pod(
        &self,
        _request: Request<RestorePodRequest>,
    ) -> Result<Response<RestorePodResponse>, Status> {
        Err(Status::unimplemented("pod checkpoint/restore is out of scope for now"))
    }
}

#[cfg(test)]
mod sandbox_filter_tests {
    use super::*;

    #[test]
    fn sandbox_state_filter_matches_status_projection() {
        for status in [PodStatus::Pending, PodStatus::Running, PodStatus::Succeeded, PodStatus::Failed, PodStatus::Unknown] {
            let ready = PodSandboxStateValue { state: PodSandboxState::SandboxReady as i32 };
            let not_ready = PodSandboxStateValue { state: PodSandboxState::SandboxNotready as i32 };
            assert!(matches_sandbox_state(None, &status));
            assert_eq!(matches_sandbox_state(Some(&ready), &status), status == PodStatus::Running);
            assert_eq!(matches_sandbox_state(Some(&not_ready), &status), status != PodStatus::Running);
        }
    }
}

#[cfg(test)]
mod implicit_cleanup_tests {
    use super::*;

    #[tokio::test]
    async fn conditional_cleanup_preserves_foreign_and_shared_sandboxes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.runtime.sandboxes.wasm_enabled = true;
        config.storage.container_dir = tmp.path().join("containers");
        let config = Arc::new(config);
        let state = Arc::new(StateManager::new(tmp.path().join("state")).unwrap());
        let registry = Arc::new(SandboxRegistry::new(&config).unwrap());
        let containers = Arc::new(ContainerManager::new(config.clone(), state.clone(), registry.clone()));
        let service = NautilusRuntimeService {
            pod_manager: Arc::new(PodManager::new(config.clone(), state.clone(), containers.clone())),
            container_manager: containers,
            sandbox_registry: registry,
            pod_net: Arc::new(PodNet::new(config.clone())),
            policy: Arc::new(crate::policy::PolicyEngine::new(&config)),
            images: Arc::new(crate::storage::ImageManager::new(config.clone())),
            vm_ready: Default::default(),
            _config: config,
            _state_manager: state,
        };
        for (namespace, has_sibling, should_remove) in [
            ("default", false, false), ("ctl", true, false), ("ctl", false, true),
        ] {
            let id = service.run_pod_sandbox(Request::new(RunPodSandboxRequest {
                config: Some(PodSandboxConfig {
                    metadata: Some(PodSandboxMetadata { namespace: namespace.into(), ..Default::default() }),
                    labels: [("nautilus.ctl".into(), "true".into())].into_iter().collect(),
                    ..Default::default()
                }),
                runtime_handler: "wasi".into(),
            })).await.unwrap().into_inner().pod_sandbox_id;
            if has_sibling {
                let module = tmp.path().join("sibling.wasm");
                std::fs::write(&module, b"\0asm\x01\0\0\0").unwrap();
                let cid = service.create_container(Request::new(CreateContainerRequest {
                    pod_sandbox_id: id.clone(),
                    config: Some(ContainerConfig {
                        metadata: Some(ContainerMetadata { name: "sibling".into(), ..Default::default() }),
                        image: Some(ImageSpec { image: module.display().to_string(), ..Default::default() }),
                        ..Default::default()
                    }),
                    ..Default::default()
                })).await.unwrap().into_inner().container_id;
                // Model the state of a sibling restarted since the client's list snapshot.
                let mut restarted = service._state_manager.get_container_state(&cid).await.unwrap().unwrap();
                restarted.id = uuid::Uuid::new_v4().to_string();
                restarted.status = ContainerStatus::Running;
                service._state_manager.save_container_state(&restarted).await.unwrap();
                for condition in ["exited", "not-running"] {
                    let mut rm = Request::new(RemoveContainerRequest { container_id: restarted.id.clone() });
                    rm.metadata_mut().insert("nautilus-remove-if", tonic::metadata::MetadataValue::from_static(condition));
                    assert_eq!(service.remove_container(rm).await.unwrap_err().code(), tonic::Code::FailedPrecondition);
                }
                assert!(service._state_manager.get_container_state(&restarted.id).await.unwrap().is_some());
            }
            let mut conditional = Request::new(RemovePodSandboxRequest { pod_sandbox_id: id.clone() });
            conditional.metadata_mut().insert("nautilus-if-implicit-empty", "true".parse().unwrap());
            service.remove_pod_sandbox(conditional).await.unwrap();
            assert_eq!(service.pod_manager.get_pod_state(&id).await.unwrap().is_none(), should_remove);
            // Unconditional CRI removes a Running sandbox and its containers; repeat is a no-op.
            for _ in 0..2 {
                service.remove_pod_sandbox(Request::new(RemovePodSandboxRequest { pod_sandbox_id: id.clone() })).await.unwrap();
            }
            assert!(service.container_manager.list_containers_in_pod(&id).await.unwrap().is_empty());
            assert!(service.pod_manager.get_pod_state(&id).await.unwrap().is_none());
        }
    }
}
