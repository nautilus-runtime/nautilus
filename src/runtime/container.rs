use crate::core::{Config, Error, Result, StateManager};
use crate::core::state::{ContainerState, ContainerStatus};
use crate::sandbox::{SandboxRegistry, StartOutcome, WorkloadSpec};
use std::collections::HashMap;
use std::time::Duration;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use tokio::sync::{RwLock, watch};


fn reap_if_exited(pid: i32) -> std::io::Result<Option<i32>> {
    loop {
        let mut status = 0;
        match unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } {
            0 => return Ok(None),
            -1 => {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            _ if libc::WIFEXITED(status) => return Ok(Some(libc::WEXITSTATUS(status))),
            _ if libc::WIFSIGNALED(status) => return Ok(Some(128 + libc::WTERMSIG(status))),
            _ => return Ok(None),
        }
    }
}

async fn wait_for_workload(pid: i32) -> std::io::Result<i32> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::fs::FileExt;
    use tokio::io::unix::AsyncFd;

    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    if raw >= 0 {
        let fd = AsyncFd::new(unsafe { OwnedFd::from_raw_fd(raw) })?;
        loop {
            let mut readiness = fd.readable().await?;
            match reap_if_exited(pid) {
                Ok(Some(code)) => return Ok(code),
                Ok(None) => readiness.clear_ready(),
                // A detached external runc init has a different parent.
                Err(e) if e.raw_os_error() == Some(libc::ECHILD) => return Ok(-1),
                Err(e) => return Err(e),
            }
        }
    }
    let error = std::io::Error::last_os_error();
    if !matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL | libc::EPERM | libc::ESRCH)) {
        return Err(error);
    }

    // Older kernels have no pidfds. Keep one proc handle for external
    // workloads so a reused pid cannot extend the lifetime we observe.
    let mut stat = None;
    loop {
        match reap_if_exited(pid) {
            Ok(Some(code)) => return Ok(code),
            Ok(None) => {}
            Err(e) if e.raw_os_error() == Some(libc::ECHILD) => {
                if stat.is_none() {
                    match std::fs::File::open(format!("/proc/{pid}/stat")) {
                        Ok(file) => stat = Some(file),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(-1),
                        Err(e) => return Err(e),
                    }
                }
                let mut buf = [0; 512];
                match stat.as_ref().unwrap().read_at(&mut buf, 0) {
                    Ok(0) => return Ok(-1),
                    Ok(n) => {
                        let state = buf[..n].rsplit(|c| *c == b')').next().and_then(|tail| tail.get(1));
                        if matches!(state, Some(b'Z' | b'X')) { return Ok(-1); }
                    }
                    Err(e) if matches!(e.raw_os_error(), Some(libc::ESRCH | libc::ENOENT)) => return Ok(-1),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

mod vm;

type NativeExits = Arc<Mutex<HashMap<(String, u32), watch::Receiver<()>>>>;

pub(crate) struct CreateWorkload<'a> {
    pub pod_id: &'a str,
    pub name: &'a str,
    pub image: &'a str,
    pub sandbox_kind: &'a str,
    pub command: Vec<String>,
    pub args: Vec<String>,
    pub log_path: &'a str,
    pub volumes: Vec<crate::sandbox::VolumeMount>,
    pub env: HashMap<String, String>,
    pub resources: Option<crate::sandbox::ResourceLimits>,
    pub security: Option<crate::sandbox::ContainerSecurity>,
    pub labels: HashMap<String, String>,
    pub annotations: HashMap<String, String>,
    pub stdin: bool,
    pub working_dir: &'a str,
    pub strict_groups: bool,
    pub tty: bool,
    pub stdin_once: bool,
    pub attempt: u32,
}

#[derive(Clone)]
pub struct ContainerManager {
    vm_operations: Arc<crate::runtime::OperationLocks>,
    native_exits: NativeExits,
    _config: Arc<Config>,
    state_manager: Arc<StateManager>,
    sandbox_registry: Arc<SandboxRegistry>,
    containers: Arc<RwLock<HashMap<String, ContainerState>>>,
}

impl ContainerManager {
    pub fn new(
        config: Arc<Config>,
        state_manager: Arc<StateManager>,
        sandbox_registry: Arc<SandboxRegistry>,
    ) -> Self {
        Self {
            vm_operations: Default::default(),
            native_exits: Default::default(),
            _config: config,
            state_manager,
            sandbox_registry,
            containers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new container. `sandbox_kind` is the resolved CRI runtime_handler
    /// inherited from the pod; resources are prepared immediately so unusable
    /// images/modules fail at create-time.
    /// Apply docker-style `-p` port publishing for a running runc container.
    /// Reads the canonical spec from the `nautilus.dev/publish` label; the pod
    /// IP comes from the pod's CNI network state.
    async fn publish_rules(&self, state: &ContainerState) {
        if state.sandbox_kind != "runc" {
            return;
        }
        let Some(spec) = state.labels.get(crate::network::publish::PUBLISH_LABEL) else {
            return;
        };
        let maps = crate::network::publish::PortMapping::parse_list(spec);
        if maps.is_empty() {
            return;
        }
        let ip = match self.state_manager.get_pod_state(&state.pod_id).await {
            Ok(Some(pod)) if !pod.network_ip.is_empty() => pod.network_ip,
            _ => {
                tracing::warn!(container_id = %state.id, "port publish skipped: no pod ip");
                return;
            }
        };
        if let Err(e) = crate::network::publish::apply(&ip, &state.id, &maps).await {
            tracing::warn!(container_id = %state.id, error = %e, "nat publish failed");
        }
    }

    pub async fn create_container(
        &self,
        pod_id: &str,
        name: &str,
        image: &str,
        sandbox_kind: &str,
        command: Vec<String>,
        args: Vec<String>,
        log_path: &str,
        volumes: Vec<crate::sandbox::VolumeMount>,
        env: HashMap<String, String>,
        resources: Option<crate::sandbox::ResourceLimits>,
        security: Option<crate::sandbox::ContainerSecurity>,
        labels: HashMap<String, String>,
        annotations: HashMap<String, String>,
        stdin: bool,
    ) -> Result<String> {
        self.create_with_process_options(CreateWorkload { pod_id, name, image, sandbox_kind, command, args,
            log_path, volumes, env, resources, security, labels, annotations, stdin, working_dir: "", strict_groups: false, tty: false, stdin_once: false, attempt: 0 }).await
    }

    pub(crate) async fn create_with_process_options(&self, request: CreateWorkload<'_>) -> Result<String> {
        let CreateWorkload { pod_id, name, image, sandbox_kind, command, args, log_path, volumes, env,
            resources, security, labels, annotations, stdin, working_dir, strict_groups, tty, stdin_once, attempt } = request;
        let container_id = StateManager::generate_container_id();
        let open_stdin = stdin;

        // CRI log convention: the kubelet gives the sandbox a log_directory
        // (`/var/log/pods/<ns>_<name>_<uid>/`) and the container a log_path
        // (`<name>/<id>.log`) relative to it. We honor that when present,
        // falling back to the legacy internal `<id>-out/out.log` location.
        let pod_log_dir = self
            .state_manager
            .get_pod_state(pod_id)
            .await
            .ok()
            .flatten()
            .map(|p| p.log_directory)
            .unwrap_or_default();
        // With a CRI log target the container writes through the shared
        // log-capture bind; keep the HOST destination file in sync by making
        // the capture dir the log file's parent and binding it.
        let mut vm = None;
        let (log_file, log_relpath, log_captured) = if sandbox_kind == "vm" {
            let target = crate::runtime::vm_log::Target::new(std::path::Path::new(&pod_log_dir), std::path::Path::new(log_path),
                &self._config.storage.container_dir, &container_id)?;
            vm = Some(crate::core::state::VmContainer { image_id: String::new(), preparing: true, logs_complete: false, relative_log: log_path.into(), cursor: Default::default() });
            (target.path().to_owned(), String::new(), false)
        } else if !pod_log_dir.is_empty() && !log_path.is_empty()
        {
            let rel = log_path.trim_start_matches('/').to_string();
            let host = std::path::Path::new(&pod_log_dir).join(&rel);
            // Pre-create the host file (and parents) root-owned, rw for all so
            // non-root containers can append.
            if let Some(parent) = host.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::File::create(&host);
            let _ = std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o666));
            let file_name = host
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "out.log".to_string());
            (host, file_name, true)
        } else {
            let d = self._config.storage.container_dir.join(format!("{container_id}-out"));
            (d.join("out.log"), "out.log".to_string(), false)
        };
        // Remember whether the capture dir is an external CRI log location.
        // Stored in the workload spec below via state.
        let _ = log_captured;

        let mut container_state = ContainerState {
            id: container_id.clone(),
            pod_id: pod_id.to_string(),
            attempt,
            name: name.to_string(),
            image: image.to_string(),
            image_id: String::new(),
            status: ContainerStatus::Created,
            created_at: chrono::Utc::now(),
            started_at: None,
            finished_at: None,
            exit_code: None,
            pid: None,
            log_path: Some(log_file.to_string_lossy().to_string()),
            sandbox_kind: sandbox_kind.to_string(),
            resources,
            security,
            env,
            wasi_args: if matches!(sandbox_kind, "wasi" | "wasm") { args.clone() } else { Vec::new() },
            labels,
            annotations,
            volumes,
            finish_reason: String::new(),
            open_stdin,
            vm,
        };

        // Prepare backend resources now (bundle / wasm compile) to fail fast.
        let mut spec = self.workload_spec_with_net(&container_state).await?;
        spec.working_dir = working_dir.to_owned();
        spec.strict_groups = strict_groups;
        spec.tty = tty;
        spec.stdin_once = stdin_once;
        spec.entrypoint = command;
        spec.args = args;
        spec.log_relpath = log_relpath;
        if sandbox_kind == "vm" {
            return self.create_guest(container_state, spec).await;
        }
        let backend = self.backend_for(&container_state)?;
        container_state.image_id = backend.prepare_image(&spec).await?;
        if let Err(error) = self.state_manager.save_container_state(&container_state).await {
            backend.remove(&container_id).await?;
            return Err(error);
        }

        {
            let mut containers = self.containers.write().await;
            containers.insert(container_id.clone(), container_state);
        }

        tracing::info!(container_id = %container_id, sandbox_kind = %sandbox_kind, "Container created");
        Ok(container_id)
    }

    fn backend_for(&self, state: &ContainerState) -> Result<Arc<dyn crate::sandbox::SandboxBackend>> {
        if state.sandbox_kind == "vm" {
            return Ok(Arc::new(crate::sandbox::vm_backend::VmBackend::new(self._config.clone(), &state.pod_id)));
        }
        self.sandbox_registry.resolve(&state.sandbox_kind)
    }

    fn workload_spec(&self, state: &ContainerState) -> Result<WorkloadSpec> {
        Ok(WorkloadSpec {
            id: state.id.clone(),
            image_ref: state.image.clone(),
            image_id: state.image_id.clone(),
            cgroup_parent: String::new(),
            args: state.wasi_args.clone(),
            entrypoint: Vec::new(),
            working_dir: String::new(),
            strict_groups: false,
            tty: false,
            stdin_once: false,
            env: state.env.clone(),
            // Host-side capture directory; RuncBackend bind-mounts it into the
            // container at /.nautilus-log. When the CRI log path points into
            // the sandbox log directory we bind THAT parent and write the
            // basename straight into the expected host file.
            output_dir: {
                let internal = self._config.storage.container_dir.as_path();
                let lp = state.log_path.as_deref().map(std::path::Path::new);
                let external = lp
                    .and_then(|p| p.parent().map(|q| q.to_path_buf()))
                    .filter(|p| !(p.starts_with(internal)));
                external.unwrap_or_else(|| {
                    self._config.storage.container_dir.join(format!("{}-out", state.id))
                })
            },
            resources: state.resources.clone(),
            security: state.security.clone(),
            network_ns: None, // filled in async wrapper below
            ipc_ns: None,
            log_relpath: state
                .log_path
                .as_deref()
                .and_then(|p| {
                    std::path::Path::new(p)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "out.log".to_string()),
            hostname: None,
            dns_servers: Vec::new(),
            dns_searches: Vec::new(),
            dns_options: Vec::new(),
            volumes: state.volumes.clone(),
            pid_ns: None,
            open_stdin: state.open_stdin,
        })
    }

    /// Async variant that resolves the pod's published netns so containers can
    /// join their pod's network namespace.
    async fn workload_spec_with_net(&self, state: &ContainerState) -> Result<WorkloadSpec> {
        let mut spec = self.workload_spec(state)?;
        if state.sandbox_kind == "runc" {
            if let Some(pod) = self.state_manager.get_pod_state(&state.pod_id).await? {
                spec.network_ns = pod.network_namespace;
                spec.cgroup_parent = pod.cgroup_parent;
                // Dedicated pod IPC ns exists only when its sysctls needed one.
                let h = crate::network::podnet::ipcns_dir().join(&state.pod_id);
                if h.exists() {
                    spec.ipc_ns = Some(h.to_string_lossy().to_string());
                }
                if !pod.hostname.is_empty() {
                    spec.hostname = Some(pod.hostname.clone());
                }
                // CRI pid=POD (default): join the pod's shared PID namespace
                // by its handle path; CONTAINER/NODE modes override via the
                // security flags checked here.
                if spec
                    .security
                    .as_ref()
                    .is_some_and(|s| s.share_pod_pid && !s.host_pid)
                    && pod.pidns_holder_pid.is_some()
                {
                    spec.pid_ns = Some(
                        crate::network::podnet::pidns_dir()
                            .join(&state.pod_id)
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                spec.dns_servers = pod.dns_servers.clone();
                spec.dns_searches = pod.dns_searches.clone();
                spec.dns_options = pod.dns_options.clone();
            }
        }
        Ok(spec)
    }

    /// Start a container by dispatching to its sandbox backend.
    /// Freshly created (Created) AND previously-run (Stopped, docker-style
    /// restart) containers are both startable.
    pub async fn start_container(&self, container_id: &str) -> Result<()> {
        let mut container_state = self.get_container_state(container_id).await?
            .ok_or_else(|| Error::Container(format!("Container {} not found", container_id)))?;

        if container_state.sandbox_kind == "vm" {
            self.guest_transition(container_id, crate::vm::workload::Operation::Start).await?;
            return Ok(());
        }

        match container_state.status {
            ContainerStatus::Created | ContainerStatus::Stopped => {}
            ref other => {
                return Err(Error::Container(format!(
                    "Container {} is not startable, current status: {:?}",
                    container_id, other
                )));
            }
        }
        // Reset bookkeeping from any previous run; backends repopulate it.
        container_state.started_at = None;
        container_state.finished_at = None;
        container_state.exit_code = None;
        container_state.pid = None;

        let backend = self.backend_for(&container_state)?;
        let spec = self.workload_spec_with_net(&container_state).await?;
        let managed = match backend.start(&spec).await? {
            StartOutcome::Detached { pid } => {
                container_state.status = ContainerStatus::Running;
                container_state.started_at = Some(chrono::Utc::now());
                container_state.pid = Some(pid);
                None
            }
            StartOutcome::Guest => {
                container_state.status = ContainerStatus::Running;
                container_state.started_at = Some(chrono::Utc::now());
                None
            }
            StartOutcome::Managed { activate, finished } => {
                container_state.status = ContainerStatus::Running;
                container_state.started_at = Some(chrono::Utc::now());
                container_state.finish_reason.clear();
                Some((activate, finished))
            }
        };

        {
            let mut containers = self.containers.write().await;
            self.state_manager.save_container_state(&container_state).await?;
            containers.insert(container_id.to_string(), container_state.clone());
        }

        if let Some((activate, finished)) = managed {
            self.spawn_managed_reaper(&container_state, finished);
            let _ = activate.send(());
        }

        self.publish_rules(&container_state).await;
        if let Some(pid) = container_state.pid.filter(|p| *p > 0) {
            self.spawn_reaper(container_id.to_string(), pid);
        }
        tracing::info!(container_id = %container_id, "Container started");
        Ok(())
    }

    fn spawn_managed_reaper(
        &self,
        state: &ContainerState,
        mut finished: tokio::sync::watch::Receiver<Option<i32>>,
    ) {
        let manager = self.clone();
        let id = state.id.clone();
        let instance = state.started_at;
        tokio::spawn(async move {
            let result = finished.wait_for(Option::is_some).await;
            let code = result.ok().and_then(|exit| *exit).unwrap_or(1);
            let mut cache = manager.containers.write().await;
            let outcome = async {
                if let Some(mut current) = manager.state_manager.get_container_state(&id).await? {
                    if current.started_at == instance && current.status == ContainerStatus::Running {
                        current.status = ContainerStatus::Stopped;
                        current.finished_at = Some(chrono::Utc::now());
                        current.exit_code = Some(code);
                        manager.state_manager.save_container_state(&current).await?;
                        cache.insert(id.clone(), current);
                    }
                }
                Ok::<_, Error>(())
            }.await;
            if let Err(error) = outcome {
                tracing::error!(container_id = %id, %error, "could not record WASI workload exit");
            }
        });
    }

    pub(crate) async fn recover_managed_workloads(&self) -> Result<()> {
        for mut state in self.state_manager.list_containers().await? {
            if matches!(state.sandbox_kind.as_str(), "wasi" | "wasm")
                && matches!(state.status, ContainerStatus::Running | ContainerStatus::Paused)
            {
                state.status = ContainerStatus::Stopped;
                state.finished_at = Some(chrono::Utc::now());
                state.exit_code = Some(137);
                state.finish_reason = "RuntimeRestarted".into();
                self.state_manager.save_container_state(&state).await?;
            }
        }
        crate::sandbox::wasm_backend::recover_staging(&self._config, &self.state_manager).await
    }

    /// Watch one detached workload pid; flip its state to Stopped with the
    /// real exit code once it exits. Guarded so a concurrent stop() result
    /// (which already persisted Stopped) is not overwritten.
    fn spawn_reaper(&self, container_id: String, pid: u32) {
        let state_manager = self.state_manager.clone();
        let cache = self.containers.clone();
        let exits = self.native_exits.clone();
        let key = (container_id.clone(), pid);
        let (done, receiver) = watch::channel(());
        exits.lock().unwrap().insert(key.clone(), receiver);
        tokio::spawn(async move {
            let code = match wait_for_workload(pid as i32).await {
                Ok(code) => code,
                Err(error) => {
                    tracing::error!(%container_id, %pid, %error, "could not watch workload exit");
                    exits.lock().unwrap().remove(&key);
                    return;
                }
            };

            let mut cache = cache.write().await;
            if let Ok(Some(mut s)) = state_manager.get_container_state(&container_id).await {
                if s.pid == Some(pid) && (s.status == ContainerStatus::Running || s.exit_code == Some(-1)) {
                    // OOM detection: killed-by-SIGKILL (128+9=137) plus an
                    // oom_kill cgroup event => reason OOMKilled.
                    let mut reason = String::new();
                    if code == 137 {
                        // The container's cgroup scope is torn down by systemd
                        // almost immediately after death, so OOM attribution
                        // relies on the start-time watcher/fallback in
                        // RuncBackend::oom_killed (see runc_backend.rs).
                        if crate::sandbox::runc_backend::oom_killed(container_id.as_str()) {
                            reason = "OOMKilled".to_string();
                        }
                    }
                    if s.status == ContainerStatus::Running
                        && s.labels.contains_key(crate::network::publish::PUBLISH_LABEL)
                    {
                        crate::network::publish::remove(&container_id).await;
                    }
                    s.status = ContainerStatus::Stopped;
                    s.finished_at = Some(chrono::Utc::now());
                    s.exit_code = Some(code);
                    if !reason.is_empty() { s.finish_reason = reason; }
                    match state_manager.save_container_state(&s).await {
                        Ok(()) => {
                            tracing::info!(container_id = %container_id, exit_code = code, reason = %s.finish_reason, "workload exited");
                            cache.insert(container_id, s);
                        }
                        Err(error) => tracing::error!(%container_id, %error, "could not record workload exit"),
                    }
                }
            }
            exits.lock().unwrap().remove(&key);
            drop(done);
        });
    }

    /// Stop a container via its backend.
    /// Run a command inside a running container via its sandbox backend.
    pub async fn exec_sync(
        &self,
        container_id: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<(i32, Vec<u8>, Vec<u8>)> {
        let state = self.get_container_state(container_id).await?
            .ok_or_else(|| Error::Container(format!("Container {} not found", container_id)))?;
        // CRI semantics: execSync requires a Running container, but a short
        // batch workload can finish between start and exec (critest asserts
        // immediately after start). Defer the liveness judgement to the
        // backend: libcontainer will refuse only when the container is truly
        // gone/not joinable, letting the fast-path race succeed.
        if state.status != ContainerStatus::Running && state.status != ContainerStatus::Stopped {
            return Err(Error::Container(format!(
                "Container {} is not running, current status: {:?}",
                container_id, state.status
            )));
        }
        let backend = self.backend_for(&state)?;
        // CRI spec: execSync with timeout aborts once the deadline lapses —
        // backends own the deadline so the exec'd process is killed and
        // reaped, not merely abandoned by a dropped future.
        backend
            .exec_sync(container_id, args, env, cwd, timeout)
            .await
    }

    /// Connect a stream to the guest identified by its owning Pod and boot.
    pub(crate) async fn guest_stream(
        &self,
        pod: &str,
        id: &str,
        operation: crate::vm::workload::Operation<'_>,
        options: crate::vm::workload::StreamOptions,
        closed: tokio::sync::watch::Receiver<bool>,
    ) -> Result<crate::vm::stream::GuestStream> {
        let owner = crate::vm::owner::PodVmStore::new(self._config.runtime.state_dir.join("vms"));
        let channel = owner.workload_channel(pod).await.map_err(|err| {
            std::io::Error::new(err.kind(), format!("connect to Pod VM guest agent: {err}"))
        })?;
        crate::vm::stream::open(channel, id, operation, options, closed).await
    }

    /// Live-stdio exec for the CRI streaming interfaces. Same container-state
    /// tolerance as `exec_sync`: a short batch workload can finish between
    /// start and exec, so only backends decide when the container is truly
    /// gone.
    pub async fn exec_stream(
        &self,
        container_id: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&str>,
    ) -> Result<crate::sandbox::ExecStreams> {
        let state = self.get_container_state(container_id).await?
            .ok_or_else(|| Error::Container(format!("Container {} not found", container_id)))?;
        if state.status != ContainerStatus::Running && state.status != ContainerStatus::Stopped {
            return Err(Error::Container(format!(
                "Container {} is not running, current status: {:?}",
                container_id, state.status
            )));
        }
        let backend = self.backend_for(&state)?;
        backend.exec_stream(container_id, args, env, cwd).await
    }

    pub async fn stop_container(&self, container_id: &str, timeout: Option<u64>) -> Result<()> {
        let container_state = self.get_container_state(container_id).await?
            .ok_or_else(|| Error::Container(format!("Container {} not found", container_id)))?;

        if container_state.sandbox_kind == "vm" {
            let ms = timeout.unwrap_or_default().checked_mul(1000).filter(|v| *v <= 300_000)
                .ok_or_else(|| Error::Experimental("guest stop timeout exceeds 300 seconds".into()))?;
            self.guest_transition(container_id, crate::vm::workload::Operation::Stop { timeout_ms: ms as i64 }).await?;
            return Ok(());
        }

        match container_state.status {
            ContainerStatus::Running => {}
            // Idempotent like docker: stopping an already-stopped container
            // succeeds without touching the backend.
            ContainerStatus::Stopped => return Ok(()),
            ref other => {
                return Err(Error::Container(format!(
                    "Container {} is not running, current status: {:?}",
                    container_id, other
                )));
            }
        }

        let backend = self.backend_for(&container_state)?;
        let grace = Duration::from_secs(timeout.unwrap_or(0));
        let exit_code = backend.stop(container_id, grace).await?;
        if exit_code.is_none() {
            if let Some(pid) = container_state.pid {
                let exit = self.native_exits.lock().unwrap().get(&(container_id.to_string(), pid)).cloned();
                if let Some(mut exit) = exit {
                    // The backend confirms termination, while waitpid supplies the actual exit code.
                    let _ = tokio::time::timeout(Duration::from_secs(1), exit.changed()).await;
                }
            }
        }
        // The exit watcher may already have recorded the actual status.
        let mut cache = self.containers.write().await;
        if let Some(mut observed) = self.state_manager.get_container_state(container_id).await? {
            if observed.status == ContainerStatus::Running {
                observed.status = ContainerStatus::Stopped;
                observed.finished_at = Some(chrono::Utc::now());
                observed.exit_code = exit_code.or(Some(-1));
                self.state_manager.save_container_state(&observed).await?;
                cache.insert(container_id.to_string(), observed);
            }
        }
        drop(cache);

        crate::network::publish::remove(container_id).await;
        // Release the parked open-stdin pipe (CRI stdin=true): EOF for any
        // stdin-reading process still alive in the container.
        crate::sandbox::runc_backend::close_container_stdin(container_id);

        tracing::info!(container_id = %container_id, "Container stopped");
        Ok(())
    }

    /// Remove a container and release backend resources.
    pub async fn remove_container(&self, container_id: &str) -> Result<()> {
        let container_state = self.get_container_state(container_id).await?
            .ok_or_else(|| Error::Container(format!("Container {} not found", container_id)))?;

        if container_state.sandbox_kind == "vm" {
            return self.remove_guest(container_id).await;
        }

        // CRI allows removing a running container; runtime force-stops it.
        if container_state.status == ContainerStatus::Running {
            self.stop_container(container_id, None).await?;
        }

        let backend = self.backend_for(&container_state)?;
        backend.remove(container_id).await?;

        // Best-effort cleanup of the output capture directory.
        let output_dir = self
            ._config
            .storage
            .container_dir
            .join(format!("{}-out", container_id));
        if tokio::fs::try_exists(&output_dir).await.unwrap_or(false) {
            if let Err(e) = tokio::fs::remove_dir_all(&output_dir).await {
                tracing::warn!(container_id = %container_id, error = %e, "failed to clean output dir");
            }
        }

        // Serialize deletion with the exit watchers so they cannot reinsert a removed workload.
        {
            let mut containers = self.containers.write().await;
            containers.remove(container_id);
            self.state_manager.delete_container_state(container_id).await?;
        }

        crate::network::publish::remove(container_id).await;
        crate::sandbox::runc_backend::close_container_stdin(container_id);

        tracing::info!(container_id = %container_id, "Container removed");
        Ok(())
    }

    /// Get container state
    pub async fn get_container_state(&self, container_id: &str) -> Result<Option<ContainerState>> {
        // First check memory cache
        {
            let containers = self.containers.read().await;
            if let Some(state) = containers.get(container_id) {
                return Ok(Some(state.clone()));
            }
        }

        // If not in cache, load from state manager
        if let Some(state) = self.state_manager.get_container_state(container_id).await? {
            // Cache it for future use
            {
                let mut containers = self.containers.write().await;
                containers.insert(container_id.to_string(), state.clone());
            }
            Ok(Some(state))
        } else {
            Ok(None)
        }
    }

    /// List all containers
    pub async fn list_containers(&self) -> Result<Vec<ContainerState>> {
        self.state_manager.list_containers().await
    }

    /// List containers in a specific pod
    pub async fn list_containers_in_pod(&self, pod_id: &str) -> Result<Vec<ContainerState>> {
        self.state_manager.list_containers_in_pod(pod_id).await
    }
}

#[cfg(test)]
mod reaper_tests {
    use super::wait_for_workload;
    use std::process::Command;
    use std::time::Duration;

    #[tokio::test]
    async fn reaps_child_and_returns_its_exit_code() {
        let mut child = Command::new("sh").args(["-c", "exit 27"]).spawn().unwrap();
        assert_eq!(wait_for_workload(child.id() as i32).await.unwrap(), 27);
        assert_eq!(child.try_wait().unwrap_err().raw_os_error(), Some(libc::ECHILD));
    }

    #[tokio::test]
    async fn waits_for_external_init_without_claiming_its_exit_code() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::Stdio;
        let mut parent = Command::new("sh")
            .args(["-c", "exec 3<&0; (read line <&3) & echo $!; wait"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut line = String::new();
        BufReader::new(parent.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let mut watcher = Box::pin(wait_for_workload(line.trim().parse().unwrap()));
        assert!(futures::poll!(&mut watcher).is_pending());
        parent.stdin.take().unwrap().write_all(b"done\n").unwrap();
        assert_eq!(watcher.await.unwrap(), -1);
        assert!(parent.wait().unwrap().success());
    }

    #[test]
    fn runtime_shutdown_leaves_a_live_workload_running() {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = Command::new("sh").args(["-c", "read line"]).stdin(Stdio::piped()).spawn().unwrap();
        let pid = child.id() as i32;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let mut watcher = Box::pin(wait_for_workload(pid));
            assert!(futures::poll!(&mut watcher).is_pending());
            tokio::spawn(watcher);
        });
        let (send, done) = std::sync::mpsc::channel();
        let shutdown = std::thread::spawn(move || { drop(runtime); send.send(()).ok(); });
        let stopped = done.recv_timeout(Duration::from_secs(2));
        let still_running = child.try_wait().unwrap().is_none();
        child.stdin.take().unwrap().write_all(b"done\n").unwrap();
        let _ = child.wait();
        shutdown.join().unwrap();
        assert!(stopped.is_ok() && still_running);
    }
}
