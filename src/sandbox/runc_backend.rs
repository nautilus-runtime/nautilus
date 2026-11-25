//! runc-backed sandbox: real OCI container lifecycle — bundles are materialized
//! by the native `oci-distribution` pull pipeline (`storage::pull`), and
//! containers run through the `runc` CLI. No docker/podman dependency.

use crate::core::{Config, Error, Result};
use crate::sandbox::{SandboxBackend, SandboxKind, StartOutcome, WorkloadSpec};
use async_trait::async_trait;
use libcontainer::container::builder::ContainerBuilder;
use libcontainer::container::{Container, ContainerStatus as OciStatus, State as OciState};
use libcontainer::error::LibcontainerError;
use libcontainer::signal::Signal;
use libcontainer::syscall::syscall::SyscallType;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::process::Command;

mod init_io;
pub(crate) use init_io::InitIo;

/// Lifecycle driver behind the runc sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Driver {
    /// In-process container lifecycle via the libcontainer library (default).
    Libcontainer,
    /// Shell out to the runc binary (fallback).
    Exec,
}

// The saved libcontainer config owns this triplet for the lifetime of the
// container. Validate the complete slice hierarchy reported by libcgroups.
fn owns_systemd_scope(container: &Container, scope: &Path) -> bool {
    container
        .spec()
        .is_ok_and(|config| matches_systemd_scope(container.id(), &config.cgroup_path, scope))
}

fn matches_systemd_scope(id: &str, configured: &Path, scope: &Path) -> bool {
    let Some(triplet) = configured.to_str() else {
        return false;
    };
    let Some((slice, unit)) = triplet.split_once(':') else {
        return false;
    };
    let Some((prefix, configured_id)) = unit.split_once(':') else {
        return false;
    };
    if configured_id != id {
        return false;
    }
    let expected_slice = match (slice, prefix) {
        ("", "youki") => "system.slice",
        (slice, "nautilus") if !slice.is_empty() => slice,
        _ => return false,
    };
    let expected_unit = format!("{prefix}-{id}.scope");
    if scope.file_name() != Some(expected_unit.as_ref()) {
        return false;
    }
    let Some(parent) = scope
        .parent()
        .and_then(|p| p.strip_prefix("/sys/fs/cgroup").ok())
    else {
        return false;
    };
    let hierarchy = Path::new("/").join(parent);
    hierarchy
        .to_str()
        .and_then(|name| crate::vm::resources::parent_slice(name).ok().flatten())
        .as_deref()
        == Some(expected_slice)
}

fn cgroup_v2_relative(path: &Path) -> Option<&Path> {
    let relative = path.strip_prefix("/").ok()?;
    (!relative.as_os_str().is_empty()
        && relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_))))
    .then_some(relative)
}

fn owned_cgroup_v2(
    id: &str,
    configured: &Path,
    membership: &str,
    systemd: bool,
) -> Option<PathBuf> {
    let root = Path::new("/sys/fs/cgroup");
    let member = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?;
    let member = root.join(cgroup_v2_relative(Path::new(member))?);
    if systemd {
        return member
            .ancestors()
            .find(|scope| matches_systemd_scope(id, configured, scope))
            .map(Path::to_owned);
    }

    let relative = cgroup_v2_relative(configured)?;
    let scope = root.join(relative);
    (relative.file_name() == Some(std::ffi::OsStr::new(id)) && member.starts_with(&scope))
        .then_some(scope)
}

// The init can exit between libcontainer's liveness check and its next
// operation. Confirm disappearance without signalling a PID or consuming
// the status owned by the workload exit watcher.
fn init_disappeared(container: &Container, error: &LibcontainerError) -> Result<bool> {
    use libcgroups::common::{AnyManagerError, WrappedIoError};
    use libcgroups::systemd::manager::SystemdManagerError;

    let Some(pid) = container.pid() else {
        return Ok(false);
    };
    let stat = PathBuf::from(format!("/proc/{}/stat", pid.as_raw()));
    if matches!(error, LibcontainerError::Procfs(_)) {
        return Ok(!stat.try_exists()?);
    }
    let LibcontainerError::CgroupManager(AnyManagerError::Systemd(SystemdManagerError::WrappedIo(
        WrappedIoError::Read { path, err },
    ))) = error
    else {
        return Ok(false);
    };
    if err.kind() != std::io::ErrorKind::NotFound
        || !owns_systemd_scope(container, path)
        || path.try_exists()?
    {
        return Ok(false);
    }
    match std::fs::read(stat) {
        Ok(bytes) => {
            // comm may contain spaces or ')'. The state follows the final ')'.
            let state = bytes
                .rsplit(|c| *c == b')')
                .next()
                .and_then(|tail| tail.get(1));
            Ok(matches!(state, Some(b'Z' | b'X')))
        }
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => Ok(true),
        Err(e) => Err(e.into()),
    }
}

fn load_lifecycle_container(root: PathBuf) -> Result<Option<Container>> {
    if !root.try_exists()? {
        return Ok(None);
    }
    let state = OciState::load(&root)
        .map_err(|e| Error::Runtime(format!("load {}: {e}", root.display())))?;
    let mut container = Container { state, root };
    if let Err(error) = container.refresh_status() {
        if !init_disappeared(&container, &error)? {
            return Err(Error::Runtime(format!("refresh {}: {error}", container.root.display())));
        }
        container.state.pid = None;
        container.set_status(OciStatus::Stopped);
    }
    Ok(Some(container))
}

#[derive(Clone)]
pub struct RuncBackend {
    config: Arc<Config>,
}

impl RuncBackend {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }

    fn driver(&self) -> Driver {
        match self.config.runtime.runc.driver.as_str() {
            "exec" => Driver::Exec,
            _ => Driver::Libcontainer,
        }
    }

    pub(crate) fn workload_cgroup_v2(&self, id: &str, pid: u32) -> Option<PathBuf> {
        if self.driver() != Driver::Libcontainer {
            return None;
        }
        let root = self.config.runtime.runc.root.join(id);
        let state = OciState::load(&root).ok()?;
        if state.id != id
            || state.pid != Some(i32::try_from(pid).ok()?)
            || state.bundle != self.bundle_dir(id).canonicalize().ok()?
            || !matches!(state.status, OciStatus::Running | OciStatus::Paused)
        {
            return None;
        }
        let container = Container { state, root };
        let config = container.spec().ok()?;
        let membership = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        owned_cgroup_v2(
            id,
            &config.cgroup_path,
            &membership,
            container.state.use_systemd,
        )
    }

    /// State directory for libcontainer-managed containers.
    fn lc_root(&self) -> PathBuf {
        let p = PathBuf::from(&self.config.runtime.runc.root);
        let _ = std::fs::create_dir_all(&p);
        p
    }

    /// In-process create+start via libcontainer.
    ///
    /// libcontainer forks the container init from the calling thread; run it on
    /// the blocking pool so async workers stay responsive and the fork happens
    /// off the hot path.
    ///
    /// The init process is exec'd with pipe stdio; the daemon side is pumped
    /// into the CRI log file by `spawn_output_logger`.
    async fn start_libcontainer(&self, spec: &WorkloadSpec) -> Result<StartOutcome> {
        let root = self.lc_root();
        let systemd = self.config.runtime.runc.systemd_cgroup;
        let id = spec.id.clone();
        let bundle = self.bundle_dir(&spec.id);
        let tty = spec.tty;

        // Pipes for the init's stdout/stderr: unlike socketpairs, pipe fds can
        // be RE-OPENED by the workload via /proc/self/fd (apache/nginx style
        // "error log = /dev/stderr" configs break with ENXIO on sockets).
        // The daemon halves are pumped into the CRI log file below.
        let (p_out, c_out) = {
            use nix::fcntl::OFlag;
            use std::os::fd::FromRawFd;
            let (r, w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)
                .map_err(|e| Error::Runtime(format!("init stdout pipe: {e}")))?;
            (
                unsafe { std::os::fd::OwnedFd::from_raw_fd(r) },
                unsafe { std::os::fd::OwnedFd::from_raw_fd(w) },
            )
        };
        let (p_err, c_err) = {
            use nix::fcntl::OFlag;
            use std::os::fd::FromRawFd;
            let (r, w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)
                .map_err(|e| Error::Runtime(format!("init stderr pipe: {e}")))?;
            (
                unsafe { std::os::fd::OwnedFd::from_raw_fd(r) },
                unsafe { std::os::fd::OwnedFd::from_raw_fd(w) },
            )
        };
        let c_out_fd = c_out;
        let c_err_fd = c_err;

        // Keep the pipe open before the first attach so readers can wait for
        // interactive input. Terminal workloads use the PTY master instead.
        let mut stdin_hold: Option<std::os::fd::OwnedFd> = None;
        let mut c_in_fd: Option<std::os::fd::OwnedFd> = None;
        if spec.open_stdin && !tty {
            use nix::fcntl::OFlag;
            use std::os::fd::FromRawFd;
            let (r, w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)
                .map_err(|e| Error::Runtime(format!("init stdin pipe: {e}")))?;
            c_in_fd = Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(r) });
            stdin_hold = Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(w) });
        }

        let log_file = spec.output_dir.join(&spec.log_relpath);

        let (pid, scope_mem_events, oom_baseline, terminal) = {
            let id = id.clone();
            tokio::task::spawn_blocking(move || crate::storage::snapshot::with_view(&bundle, || -> Result<_> {
            let lc = |e: libcontainer::error::LibcontainerError| {
                Error::Runtime(format!("libcontainer {id}: {e}"))
            };
            // Restart support: a previous (stopped) instance with this id must
            // go before the builder creates a fresh one. A just-SIGKILLed
            // instance can linger briefly — poll for death; a genuinely live
            // instance (double-start) is refused.
            use libcontainer::container::ContainerStatus as LcStatus;
            let state_dir = root.join(&id);
            if let Ok(mut prev) = Container::load(state_dir) {
                let _ = prev.refresh_status();
                for _ in 0..30 {
                    if matches!(prev.status(), LcStatus::Stopped) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    let _ = prev.refresh_status();
                }
                match prev.status() {
                    LcStatus::Stopped => {
                        prev.delete(true).map_err(|e| {
                            Error::Runtime(format!("libcontainer {id}: stale delete: {e}"))
                        })?;
                    }
                    _ => {
                        return Err(Error::Runtime(format!(
                            "libcontainer {id}: previous instance still alive"
                        )));
                    }
                }
            }

            let mut builder = ContainerBuilder::new(id.clone(), SyscallType::Linux)
                .with_root_path(&root)
                .map_err(|e| Error::Runtime(format!("libcontainer {id}: root path: {e}")))?
                .with_stdout(c_out_fd)
                .with_stderr(c_err_fd);
            if let Some(fd) = c_in_fd {
                builder = builder.with_stdin(fd);
            }
            let (mut container, terminal) = builder
                .as_init(bundle.clone())
                .with_detach(!tty)
                .with_systemd(systemd)
                .build()
                .map_err(|e| Error::Runtime(format!("libcontainer {id}: create: {e}")))?;
            if tty && terminal.is_none() {
                container.delete(true).map_err(lc)?;
                return Err(Error::Runtime(format!("libcontainer {id}: missing init PTY")));
            }
            // The created init is already in its cgroup, but cannot execute user
            // code until start(). Capture its own path and the parent counter now.
            let pid = container.pid().map(|p| p.as_raw() as u32).unwrap_or(0);
            let scope = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok().and_then(|text| {
                let rel = text.lines().find_map(|line| line.strip_prefix("0::/"))?;
                Some(Path::new("/sys/fs/cgroup").join(rel).join("memory.events"))
            });
            let baseline = scope.as_deref().and_then(|file| {
                let events = file.parent()?.parent()?.join("memory.events");
                Some((events.clone(), read_oom_kill(&events)?))
            });
            container.start().map_err(|e| {
                Error::Runtime(format!("libcontainer {id}: start: {e}"))
            })?;
            container.refresh_status().map_err(lc)?;
            // AppArmor is applied by libcontainer's init process from the spec
            // profile before the workload execve; no host-side
            // attr write needed here.
            Ok((pid, scope, baseline, terminal))
            }))
            .await
            .map_err(|e| Error::Runtime(format!("libcontainer worker join failed: {e}")))?
        }?;

        let terminal = terminal.map(|fd| Arc::new(std::fs::File::from(fd)));
        let stdin = if spec.open_stdin && tty {
            terminal.clone()
        } else {
            stdin_hold.map(|fd| Arc::new(std::fs::File::from(fd)))
        };
        let io = Arc::new(InitIo::new(pid, stdin, terminal.clone(), spec.stdin_once));
        if let Some(terminal) = terminal {
            Self::spawn_output_logger(terminal, &log_file, "stdout", io.clone());
            io.stderr.finish();
        } else {
            Self::spawn_output_logger(
                Arc::new(std::fs::File::from(p_out)),
                &log_file,
                "stdout",
                io.clone(),
            );
            Self::spawn_output_logger(
                Arc::new(std::fs::File::from(p_err)),
                &log_file,
                "stderr",
                io.clone(),
            );
        }
        init_ios().lock().unwrap().insert(id, io);

        // The persistent Pod parent also accounts for OOMs from containers
        // whose scopes are removed before the watcher can observe their exit.
        let oom_id = spec.id.clone();
        if let Some(baseline) = oom_baseline {
            oom_baselines().lock().unwrap().insert(spec.id.clone(), baseline);
        }
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
            loop {
                match scope_mem_events.as_deref().and_then(read_oom_kill) {
                    Some(n) if n > 0 => {
                        oom_hits().lock().unwrap().insert(oom_id);
                        return;
                    }
                    Some(_) => {}
                    None => return, // scope gone (container exited)
                }
                if std::time::Instant::now() > deadline {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
        });

        tracing::info!(container_id = %spec.id, pid, "libcontainer started");
        Ok(StartOutcome::Detached { pid })
    }

    /// Pump one init stdio pipe into the CRI log file, formatting each line as
    /// `<RFC3339Z> <stream> F <line>` (CRI log format, matching what the
    /// previous shell wrapper produced).
    ///
    /// The file is opened per flushed line (O_APPEND|O_CREATE): recreations of
    /// the path (ReopenContainerLog / rotation) are picked up on the next line
    /// instead of writing into an unlinked inode.
    fn spawn_output_logger(
        pipe: Arc<std::fs::File>,
        log_file: &std::path::Path,
        stream: &'static str,
        io: Arc<InitIo>,
    ) {
        let log_file = log_file.to_path_buf();
        tracing::info!(stream, file = %log_file.display(), "output logger spawned");
        // Deliberately a BLOCKING thread with plain read(2): the pipe must not
        // be registered with the tokio I/O driver (epoll) — the driver's state
        // is inherited by the forked container init and a post-fork
        // registration/interaction was observed to wedge the gRPC accept loop.
        // A dedicated thread blocks harmlessly; EOF arrives when the workload
        // dies (all write halves close).
        std::thread::spawn(move || {
            use std::io::Read;

            let mut pipe = &*pipe;
            let output = if stream == "stdout" { &io.stdout } else { &io.stderr };
            let mut buf = [0u8; 8192];
            let mut pending: Vec<u8> = Vec::new();
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break,
                    // The daemon forks constantly (SIGCHLD lands on arbitrary
                    // threads); a retried read is required or buffered output
                    // is silently dropped.
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                    Ok(n) => {
                        output.send(&buf[..n]);
                        pending.extend_from_slice(&buf[..n]);
                    }
                }
                // Flush every complete line; hold the partial tail back.
                while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=pos).collect();
                    Self::write_log_line(&log_file, stream, &line[..line.len() - 1]);
                }
            }
            if !pending.is_empty() {
                Self::write_log_line(&log_file, stream, &pending);
            }
            output.finish();
        });
    }

    /// Append one formatted CRI log line. Per-call open keeps rotation correct.
    fn write_log_line(log_file: &std::path::Path, stream: &str, line: &[u8]) {
        use std::io::Write;
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file)
        {
            // A single append keeps records from the two logger threads together.
            let record = format!("{ts} {stream} F {}\n", String::from_utf8_lossy(line));
            let _ = f.write_all(record.as_bytes());
        }
    }

    /// Observe or signal the current instance through its owning runtime.
    async fn signal_if_running(&self, id: &str, signal: Option<&'static str>) -> Result<bool> {
        if self.driver() == Driver::Exec {
            let mut cmd = Command::new(&self.config.runtime.runc.path);
            cmd.args(["state", id]);
            let out = cmd.output().await?;
            if !out.status.success() {
                let message = String::from_utf8_lossy(&out.stderr);
                if message.contains("does not exist") {
                    return Ok(false);
                }
                return Err(Error::Runtime(format!("runc state {id}: {message}")));
            }
            let state: serde_json::Value = serde_json::from_slice(&out.stdout)?;
            if state["status"] == "stopped" {
                return Ok(false);
            }
            if let Some(sig) = signal {
                let mut cmd = Command::new(&self.config.runtime.runc.path);
                cmd.arg("kill");
                if sig == "KILL" {
                    cmd.arg("--all");
                }
                cmd.args([id, sig]);
                self.run_capture(cmd, &format!("runc kill {id} {sig}")).await?;
            }
            return Ok(true);
        }

        let path = self.lc_root().join(id);
        tokio::task::spawn_blocking(move || -> Result<bool> {
            let Some(mut container) = load_lifecycle_container(path)? else {
                return Ok(false);
            };
            if !container.can_kill() {
                return Ok(false);
            }
            if let Some(name) = signal {
                let sig = Signal::try_from(if name == "KILL" { "SIGKILL" } else { "SIGTERM" })
                    .map_err(|e| Error::Runtime(format!("signal {name}: {e:?}")))?;
                if let Err(error) = container.kill(sig, name == "KILL") {
                    if matches!(error, LibcontainerError::IncorrectStatus(OciStatus::Stopped))
                        || init_disappeared(&container, &error)?
                    {
                        return Ok(false);
                    }
                    return Err(Error::Runtime(format!("kill {}: {error}", container.root.display())));
                }
            }
            Ok(true)
        }).await.map_err(|e| Error::Runtime(format!("libcontainer worker: {e}")))?
    }

    pub(crate) async fn has_stopped(&self, id: &str) -> Result<bool> {
        Ok(!self.signal_if_running(id, None).await?)
    }

    async fn wait_stopped(&self, id: &str, timeout: std::time::Duration) -> Result<bool> {
        let started = tokio::time::Instant::now();
        loop {
            if self.has_stopped(id).await? {
                return Ok(true);
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Ok(false);
            }
            tokio::time::sleep(remaining.min(std::time::Duration::from_millis(50))).await;
        }
    }

    async fn send_stop_signal(&self, id: &str, signal: &'static str) -> Result<bool> {
        if self.driver() == Driver::Libcontainer {
            return self.signal_if_running(id, Some(signal)).await;
        }
        match self.signal_if_running(id, Some(signal)).await {
            Err(_) if !self.signal_if_running(id, None).await? => Ok(false),
            result => result,
        }
    }

    async fn remove_libcontainer(&self, id: &str) -> Result<()> {
        let path = self.lc_root().join(id);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let Some(mut container) = load_lifecycle_container(path)? else {
                return Ok(());
            };
            if let Err(error) = container.delete(true) {
                if !init_disappeared(&container, &error)? {
                    return Err(Error::Runtime(format!("delete {}: {error}", container.root.display())));
                }
                // delete also refreshes internally. Its first step raced with
                // exit, so it still needs to release cgroups and the state dir.
                container.state.pid = None;
                container.delete(true)
                    .map_err(|e| Error::Runtime(format!("delete {}: {e}", container.root.display())))?;
            }
            Ok(())
        }).await.map_err(|e| Error::Runtime(format!("libcontainer worker: {e}")))?
    }

    /// Run runc for a lifecycle call that leaves a long-lived `runc init`
    /// behind (`create`). The init process inherits runc's stdio; if those are
    /// pipes, `.output()` blocks on EOF until the container dies. Point them at
    /// files instead and surface failures through the captured log.
    fn run_runc_lifecycle(
        &self,
        bundle: &Path,
        args: &[&str],
        what: &str,
    ) -> Result<()> {
        use std::fs::OpenOptions;
        use std::process::Stdio;

        let log_path = bundle.join("runc.log");
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| Error::Runtime(format!("open {}: {e}", log_path.display())))?;

        let mut cmd = std::process::Command::new(self.config.runtime.runc.path.clone());
        if self.config.runtime.runc.systemd_cgroup {
            cmd.arg("--systemd-cgroup");
        }
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));

        let status = cmd
            .status()
            .map_err(|e| Error::Runtime(format!("failed to spawn {what}: {e}")))?;
        if !status.success() {
            let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
            let tail = tail.lines().rev().take(5).collect::<Vec<_>>().join("; ");
            return Err(Error::Runtime(format!("{what} failed: {tail}")));
        }
        Ok(())
    }

    fn bundle_dir(&self, id: &str) -> PathBuf {
        PathBuf::from(&self.config.storage.container_dir).join(id)
    }

    async fn run_capture(&self, mut cmd: Command, what: &str) -> Result<String> {
        let output = cmd
            .output()
            .await
            .map_err(|e| Error::Runtime(format!("failed to spawn {what}: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(Error::Runtime(format!(
                "{what} failed ({}): {}{}",
                output.status,
                stderr.trim(),
                stdout.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }


    /// Write a minimal but valid OCI runtime spec (config.json).
    ///
    /// The workload is exec'd directly; output capture happens through stdio
    /// pipes wired in `start_libcontainer` (see `spawn_output_logger`).
    #[allow(clippy::too_many_arguments)]
    fn write_spec(
        &self,
        bundle_id: &str,
        _bundle: &Path,
        cgroup_parent: &str,
        args: Vec<String>,
        env: Vec<String>,
        cwd: &str,
        image_user: &str,
        volumes: &[crate::sandbox::VolumeMount],
        hostname: Option<&str>,
        dns_servers: &[String],
        dns_searches: &[String],
        dns_options: &[String],
        resources: Option<&crate::sandbox::ResourceLimits>,
        network_ns: Option<&str>,
        ipc_ns: Option<&str>,
        pid_ns: Option<&str>,
        security: Option<&crate::sandbox::ContainerSecurity>,
        tty: bool,
    ) -> Result<()> {
        // The workload is exec'd DIRECTLY (no shell wrapper): it is pid 1 of
        // the container (CRI semantics — ContainerPID conformance requires
        // /proc/1 to be the workload) and its exit code propagates natively
        // through the reaper's waitpid. Output capture is done by the daemon
        // through stdio pipes in start_libcontainer; AppArmor is applied by
        // libcontainer's init process from the spec profile.
        if args.is_empty() {
            return Err(Error::Runtime(
                "image has no command to exec".to_string(),
            ));
        }
        let proc_args: Vec<String> = args.clone();

        let env_full = {
            let mut e = vec![
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
            ];
            for item in env {
                if item.starts_with("PATH=") {
                    e[0] = item;
                } else {
                    e.push(item);
                }
            }
            e
        };

        // Join the pod's network namespace when one is published; otherwise the
        // container runs on the host network (CNI-less fallback).
        let mut namespaces: Vec<serde_json::Value> = Vec::new();
        let host_net = security.is_some_and(|s| s.host_network);
        let host_pid = security.is_some_and(|s| s.host_pid);
        let host_ipc = security.is_some_and(|s| s.host_ipc);
        let share_pod_pid = security.is_some_and(|s| s.share_pod_pid);
        // OCI: an ABSENT namespace type means "host namespace" (run on the
        // node) — exactly CRI's NODE/Host* mode semantics.
        if !host_pid {
            if share_pod_pid {
                // Resolved to the real handle path by the __POD_PIDNS__
                // substitution below.
                namespaces.push(serde_json::json!({"type": "pid", "path": "__POD_PIDNS__"}));
            } else {
                namespaces.push(serde_json::json!({"type": "pid"}));
            }
        }
        if !host_ipc {
            // Fresh IPC namespace by default; replaced by the pod's dedicated
            // one when its sysctls created it (see run_pod_sandbox).
            namespaces.push(serde_json::json!({"type": "ipc"}));
        }
        namespaces.push(serde_json::json!({"type": "uts"}));
        namespaces.push(serde_json::json!({"type": "mount"}));
        if let Some(ns) = network_ns {
            if !host_net {
                namespaces.push(serde_json::json!({"type": "network", "path": ns}));
            }
        } else if !host_net {
            // No published netns (host-network fallback): stay host unless a
            // fresh network namespace was explicitly requested.
            namespaces.push(serde_json::json!({"type": "network"}));
        }
        if let Some(ns) = ipc_ns {
            for n in namespaces.iter_mut() {
                if n["type"] == "ipc" {
                    *n = serde_json::json!({"type": "ipc", "path": ns});
                }
            }
        }
        if let Some(ns) = pid_ns {
            for n in namespaces.iter_mut() {
                if n["type"] == "pid" && n["path"] == "__POD_PIDNS__" {
                    *n = serde_json::json!({"type": "pid", "path": ns});
                }
            }
            namespaces.retain(|n| n["path"] != "__POD_PIDNS__");
        }

        // Write /etc/hosts + /etc/resolv.conf into the container rootfs so
        // DNS/hostname conformance checks observe the sandbox values.
        let bundle = self.bundle_dir(bundle_id);
        let rootfs = bundle.join("rootfs");
        let image_root = pathrs::Root::open(&rootfs)?;
        let (uid, gid) = crate::storage::user::resolve(&image_root, image_user, security)?;
        {
            let mut hosts = String::from("127.0.0.1	localhost
");
            if let Some(hn) = hostname {
                hosts.push_str(&format!(
                    "::1	localhost ip6-localhost ip6-loopback
{}	{}
",
                    "127.0.0.1", hn
                ));
            }
            crate::storage::unpack::write_etc_file(&image_root, "etc/hosts", hosts.as_bytes())?;
            if !dns_servers.is_empty() {
                let mut rc = String::new();
                if !dns_searches.is_empty() {
                    rc.push_str(&format!("search {}\n", dns_searches.join(" ")));
                }
                for ns in dns_servers {
                    rc.push_str(&format!("nameserver {ns}\n"));
                }
                if !dns_options.is_empty() {
                    rc.push_str(&format!("options {}\n", dns_options.join(" ")));
                }
                crate::storage::unpack::write_etc_file(&image_root, "etc/resolv.conf", rc.as_bytes())?;
            }
        }

        let mut spec = serde_json::json!({
            "ociVersion": "1.0.2",
            "process": {
                "terminal": tty,
                "user": {"uid": uid, "gid": gid},
                "args": proc_args,
                "env": env_full,
                "cwd": cwd,
                "capabilities": {
                    "bounding": DEFAULT_CAPS,
                    "effective": DEFAULT_CAPS,
                    "inheritable": DEFAULT_CAPS,
                    "permitted": DEFAULT_CAPS,
                },
            },
            "root": {
                "path": "rootfs"
            },
            "hostname": "nautilus",
            "mounts": [
                // Baseline pseudo-filesystems — without these most real
                // workloads (ps, df, dns, /dev/null…) misbehave.
                {
                    "destination": "/proc",
                    "type": "proc",
                    "source": "proc"
                },
                {
                    "destination": "/dev",
                    "type": "tmpfs",
                    "source": "tmpfs",
                    "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]
                },
                {
                    // Docker-parity: databases (postgres) allocate POSIX shm here.
                    "destination": "/dev/shm",
                    "type": "tmpfs",
                    "source": "shm",
                    "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"]
                },
                {
                    "destination": "/sys",
                    "type": "sysfs",
                    "source": "sysfs",
                    "options": ["nosuid", "noexec", "nodev", "ro"]
                }
            ],
            "linux": {
                "namespaces": namespaces
            },
            "hostname": hostname.unwrap_or("nautilus")
        });
        if tty {
            // libcontainer resolves /dev/ptmx through this container's devpts.
            spec["mounts"].as_array_mut().unwrap().push(serde_json::json!({
                "destination": "/dev/pts",
                "type": "devpts",
                "source": "devpts",
                "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620", "gid=5"]
            }));
        }
        // Bidirectional (rshared) volumes only propagate container-made mounts
        // back to the host when the container's mount root is SHARED as well;
        // youki defaults the root to slave, which is one-way.
        if volumes.iter().any(|v| v.propagation == "rshared") {
            spec["linux"]["rootfsPropagation"] = serde_json::json!("rshared");
        }
        // CRI host-path volumes.
        for v in volumes {
            // Propagation requires the HOST side of a bind to carry the shared
            // flag, otherwise mounts made inside the container never reach
            // back. Docker/containerd do exactly this upfront.
            // rshared needs the HOST side shared so container mounts reach
            // back; rslave (host->container) works from a private source too.
            if v.propagation == "rshared" {
                let _ = std::process::Command::new("mount")
                    .args(["--make-rshared", &v.host_path.to_string_lossy()])
                    .output();
            }
            // rbind (not plain bind): submounts of the host path (e.g. a tmpfs
            // the kubelet mounted inside the volume) must appear in the
            // container — critest readonly-mounts relies on it.
            let mut options = vec![
                "rbind".to_string(),
                if v.readonly { "ro" } else { "rw" }.to_string(),
            ];
            // Propagation per the CRI request (rprivate/rslave/rshared);
            // anything else the OCI layer may not parse falls back to
            // rprivate, the CRI default.
            match v.propagation.as_str() {
                "rslave" | "rshared" => options.push(v.propagation.clone()),
                _ => options.push("rprivate".to_string()),
            }
            spec["mounts"].as_array_mut().ok_or_else(|| {
                Error::Runtime("spec mounts not an array".to_string())
            })?.push(serde_json::json!({
                "destination": v.container_path,
                "type": "bind",
                "source": v.host_path.to_string_lossy(),
                "options": options
            }));
        }

        if !cgroup_parent.is_empty() {
            spec["linux"]["cgroupsPath"] = if self.config.runtime.runc.systemd_cgroup {
                let slice = crate::vm::resources::parent_slice(cgroup_parent)?
                    .ok_or_else(|| Error::Config("missing systemd cgroup parent".into()))?;
                serde_json::json!(format!("{slice}:nautilus:{bundle_id}"))
            } else {
                let parent = Path::new(cgroup_parent);
                if !parent.is_absolute() || parent.components().any(|p| matches!(p, std::path::Component::ParentDir)) {
                    return Err(Error::Config("invalid cgroupfs parent".into()));
                }
                serde_json::json!(parent.join(bundle_id))
            };
        }

        // Security context: per-request overrides over daemon defaults
        // (noNewPrivileges, readonly rootfs, seccomp profile resolution).
        crate::sandbox::spec::apply_security(&mut spec, security, &self.config.security);

        // CRI resource constraints → cgroups (cpu shares/quota/period, cpusets,
        // memory limit). Only set keys the caller actually constrained; runc
        // rejects nulls inside resource blocks.
        if let Some(r) = resources.filter(|r| !r.is_empty()) {
            let mut cpu = serde_json::Map::new();
            if r.cpu_shares > 0 {
                cpu.insert("shares".into(), serde_json::json!(r.cpu_shares));
            }
            if r.cpu_quota != 0 {
                cpu.insert("quota".into(), serde_json::json!(r.cpu_quota));
            }
            if r.cpu_period != 0 {
                cpu.insert("period".into(), serde_json::json!(r.cpu_period));
            }
            if !r.cpuset_cpus.is_empty() {
                cpu.insert("cpusetCpus".into(), serde_json::json!(r.cpuset_cpus));
            }
            if !r.cpuset_mems.is_empty() {
                cpu.insert("cpusetMems".into(), serde_json::json!(r.cpuset_mems));
            }
            let mut res = serde_json::Map::new();
            if !cpu.is_empty() {
                res.insert("cpu".into(), serde_json::Value::Object(cpu));
            }
            if r.memory_limit_in_bytes > 0 {
                res.insert(
                    "memory".into(),
                    serde_json::json!({ "limit": r.memory_limit_in_bytes }),
                );
            }
            if !res.is_empty() {
                spec["linux"]["resources"] = serde_json::Value::Object(res);
            }
        }

        std::fs::write(bundle.join("config.json"), serde_json::to_vec_pretty(&spec)?)
            .map_err(|e| Error::Runtime(format!("write config.json: {e}")))?;
        Ok(())
    }

    /// Read a field from `runc state` JSON (exec driver only; libcontainer has
    /// its own state store).
    async fn state_field(&self, id: &str, field: &str) -> Result<Option<serde_json::Value>> {
        let mut cmd = Command::new(self.config.runtime.runc.path.clone());
        cmd.args(["state", id]);
        let out = match self.run_capture(cmd, &format!("runc state {id}")).await {
            Ok(o) => o,
            Err(_) => return Ok(None), // not found / already gone
        };
        let v: serde_json::Value = serde_json::from_str(&out)
            .map_err(|e| Error::Runtime(format!("parse runc state: {e}")))?;
        Ok(v.get(field).cloned())
    }
}

/// OOM attribution state for the last-started runc containers:
/// `OOM_BASELINE` records the system-slice `oom_kill` counter at container
/// start; `OOM_HIT` is set by the per-container watcher that observes the
/// container's own `memory.events` while the scope still exists. The reaper
/// consults both when a workload dies with 137.
fn oom_baselines() -> &'static std::sync::Mutex<std::collections::HashMap<String, (PathBuf, u64)>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, (PathBuf, u64)>>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn oom_hits() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Parked senders for CRI stdin=true containers: the daemon keeps the init's
/// stdin pipe write-half open (through the holder thread) so stdin-reading
/// workloads (attach shells) block instead of EOF-exiting before a client
/// attaches. Dropping the sender closes the pipe.
type InitIoMap = std::sync::Mutex<std::collections::HashMap<String, Arc<InitIo>>>;

fn init_ios() -> &'static InitIoMap {
    static IOS: std::sync::OnceLock<InitIoMap> = std::sync::OnceLock::new();
    IOS.get_or_init(InitIoMap::default)
}

pub(crate) fn init_io(container_id: &str, pid: u32) -> Option<Arc<InitIo>> {
    init_ios()
        .lock()
        .unwrap()
        .get(container_id)
        .filter(|io| io.pid == pid)
        .cloned()
}

pub fn close_container_stdin(container_id: &str) {
    if let Some(io) = init_ios().lock().unwrap().get(container_id) {
        io.close_stdin();
    }
}

pub(crate) fn remove_init_io(container_id: &str) {
    init_ios().lock().unwrap().remove(container_id);
}

/// Parse the `oom_kill` counter out of a cgroup v2 `memory.events`.
pub(crate) fn read_oom_kill(path: &std::path::Path) -> Option<u64> {
    let txt = std::fs::read_to_string(path).ok()?;
    for line in txt.lines() {
        if let Some(rest) = line.strip_prefix("oom_kill ") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// True when the container died from the kernel OOM killer (see reaper).
pub fn oom_killed(container_id: &str) -> bool {
    if oom_hits().lock().unwrap().remove(container_id) {
        return true;
    }
    // Fallback: the container's own cgroup may already be torn down by the
    // time the reaper runs; compare against the parent cgroup counter captured
    // at start (race-free — the slice outlives every scope).
    let baseline = oom_baselines().lock().unwrap().remove(container_id);
    if let Some((events, base)) = baseline {
        if let Some(now) = read_oom_kill(&events) {
            if now > base {
                return true;
            }
        }
    }
    false
}

/// Docker-parity default capability set. Explicitly declared so that BOTH the
/// container init and every tenant exec get identical, predictable caps
/// (an absent capabilities block means "all" for init but "none" for tenants,
/// which broke e.g. `nginx -t` port binds).
const DEFAULT_CAPS: [&str; 14] = [
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FSETID",
    "CAP_FOWNER",
    "CAP_MKNOD",
    "CAP_NET_RAW",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETFCAP",
    "CAP_SETPCAP",
    "CAP_NET_BIND_SERVICE",
    "CAP_SYS_CHROOT",
    "CAP_KILL",
    "CAP_AUDIT_WRITE",
];


impl RuncBackend {
    /// Shared body of prepare(), split so failure cleanup can wrap it.
    async fn prepare_inner(&self, bundle: &Path, spec: &WorkloadSpec) -> Result<String> {
        // A bidirectional host mount needs the daemon's original shared
        // propagation topology. Preserve it with the ordinary OCI bundle.
        let overlay = !spec.volumes.iter().any(|v| v.propagation == "rshared");
        let w = crate::storage::pull::prepare_bundle(&self.config, &spec.image_ref, bundle, overlay).await?;
        let (entrypoint, cmd, img_env, workdir) = (w.entrypoint, w.cmd, w.env, w.working_dir);
        // docker/k8s-compatible argv merge:
        //   explicit entrypoint  → [entrypoint…, args…]
        //   no override, no args → image entrypoint + image cmd (defaults)
        //   no override, image has entrypoint → [image-entrypoint…, args…]
        //   bare args            → [args…] (replaces a CMD-only default)
        let merged_args: Vec<String> = if !spec.entrypoint.is_empty() {
            let mut v = spec.entrypoint.clone();
            v.extend(spec.args.iter().cloned());
            v
        } else if spec.args.is_empty() {
            let mut v = entrypoint.clone();
            v.extend(cmd);
            v
        } else if !entrypoint.is_empty() {
            let mut v = entrypoint.clone();
            v.extend(spec.args.iter().cloned());
            v
        } else {
            spec.args.clone()
        };
        // CRI/ctl-requested env wins over the image config on key collision.
        let mut full_env = img_env;
        for (k, v) in &spec.env {
            let kv = format!("{k}={v}");
            match full_env.iter().position(|e| {
                e.split('=').next().map(str::to_string).as_deref() == Some(k.as_str())
            }) {
                Some(i) => full_env[i] = kv,
                None => full_env.push(kv),
            }
        }

        // Scratch-style images ship no shell; detect once for both the exec
        // strategy and the log-capture mount.
        let backend = self.clone();
        let spec = spec.clone();
        let bundle = bundle.to_owned();
        let id = spec.id.clone();
        let image = spec.image_ref.clone();
        tokio::task::spawn_blocking(move || crate::storage::snapshot::with_view(&bundle, || {
        backend.write_spec(
            &spec.id,
            &bundle,
            &spec.cgroup_parent,
            merged_args,
            full_env,
            &workdir,
            &w.user,
            &spec.volumes,
            spec.hostname.as_deref(),
            &spec.dns_servers,
            &spec.dns_searches,
            &spec.dns_options,
            spec.resources.as_ref(),
            spec.network_ns.as_deref(),
            spec.ipc_ns.as_deref(),
            spec.pid_ns.as_deref(),
            spec.security.as_ref(),
            spec.tty,
        )
        })).await.map_err(|e| Error::Runtime(format!("prepare rootfs view: {e}")))??;
        tracing::info!(container_id = %id, image = %image, "runc bundle prepared");
        Ok(w.image_id)
    }
}

#[async_trait]
impl SandboxBackend for RuncBackend {
    fn kind(&self) -> SandboxKind {
        SandboxKind::Runc
    }

    async fn prepare(&self, spec: &WorkloadSpec) -> Result<()> {
        self.prepare_image(spec).await.map(drop)
    }

    async fn prepare_image(&self, spec: &WorkloadSpec) -> Result<String> {
        if spec.tty && self.driver() == Driver::Exec {
            return Err(Error::Runtime("native init TTY requires the libcontainer driver".into()));
        }
        let bundle = self.bundle_dir(&spec.id);
        crate::storage::snapshot::begin(&bundle)?;
        let result = async {
            fs::create_dir(bundle.join("rootfs")).await?;
            fs::create_dir_all(&spec.output_dir).await?;
            self.prepare_inner(&bundle, spec).await
        }.await;
        if result.is_err() {
            self.remove_bundle(bundle).await?;
            let internal = self.config.storage.container_dir.join(format!("{}-out", spec.id));
            if spec.output_dir == internal && fs::try_exists(&internal).await? {
                fs::remove_dir_all(internal).await?;
            }
        }
        result
    }

    async fn start(&self, spec: &WorkloadSpec) -> Result<StartOutcome> {
        match self.driver() {
            Driver::Libcontainer => self.start_libcontainer(spec).await,
            Driver::Exec => self.start_exec(spec).await,
        }
    }

    async fn stop(&self, id: &str, grace: std::time::Duration) -> Result<Option<i32>> {
        if !grace.is_zero()
            && (!self.send_stop_signal(id, "TERM").await? || self.wait_stopped(id, grace).await?)
        {
            return Ok(None);
        }
        if self.send_stop_signal(id, "KILL").await?
            && !self.wait_stopped(id, std::time::Duration::from_secs(5)).await?
        {
            return Err(Error::Runtime(format!("container {id} still running after SIGKILL")));
        }
        Ok(None)
    }

    async fn remove(&self, id: &str) -> Result<()> {
        let bundle = self.bundle_dir(id);
        let lock_path = bundle.clone();
        let guard = tokio::task::spawn_blocking(move || crate::storage::snapshot::view_lock(&lock_path))
            .await.map_err(|e| Error::Runtime(format!("lock rootfs: {e}")))??;
        match self.driver() {
            Driver::Libcontainer => self.remove_libcontainer(id).await,
            Driver::Exec => self.remove_exec(id).await,
        }?;
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            crate::storage::snapshot::remove_locked(&bundle)
        }).await.map_err(|e| Error::Runtime(format!("remove rootfs: {e}")))?
    }

    async fn exec_sync(
        &self,
        id: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&str>,
        timeout: Option<std::time::Duration>,
    ) -> Result<(i32, Vec<u8>, Vec<u8>)> {
        match self.driver() {
            Driver::Libcontainer => {
                self.exec_sync_libcontainer(id, args, env, cwd, timeout)
                    .await
            }
            Driver::Exec => self.exec_sync_exec(id, args, env, cwd, timeout).await,
        }
    }

    async fn exec_stream(
        &self,
        id: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&str>,
    ) -> Result<crate::sandbox::ExecStreams> {
        match self.driver() {
            Driver::Libcontainer => self.exec_stream_libcontainer(id, args, env, cwd).await,
            Driver::Exec => Err(Error::Runtime(
                "exec_stream is not supported for the exec driver".to_string(),
            )),
        }
    }
}

impl RuncBackend {
    /// Read the container's exec identity from its bundle config.json:
    /// uid/gid, supplementary groups, AppArmor profile and noNewPrivileges.
    /// Unset values when the state/bundle is unreadable.
    fn container_identity(
        root: &std::path::Path,
        id: &str,
    ) -> (Option<u32>, Option<u32>, Vec<u32>, Option<String>, Option<bool>) {
        let bundle = std::fs::read_to_string(root.join(id).join("state.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("bundle").and_then(|b| b.as_str()).map(String::from));
        bundle
            .map(|dir| std::path::PathBuf::from(dir).join("config.json"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| {
                v.get("process").map(|p| {
                    let gs: Vec<u32> = p
                        .get("user")
                        .and_then(|u| u.get("supplementaryGroups"))
                        .and_then(|x| x.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_u64().map(|x| x as u32))
                                .collect()
                        })
                        .unwrap_or_default();
                    (
                        p.get("user")
                            .and_then(|u| u.get("uid"))
                            .and_then(|x| x.as_u64())
                            .map(|x| x as u32),
                        p.get("user")
                            .and_then(|u| u.get("gid"))
                            .and_then(|x| x.as_u64())
                            .map(|x| x as u32),
                        gs,
                        p.get("apparmorProfile")
                            .and_then(|x| x.as_str())
                            .map(String::from),
                        p.get("noNewPrivileges").and_then(|x| x.as_bool()),
                    )
                })
            })
            .unwrap_or((None, None, Vec::new(), None, None))
    }

    /// libcontainer-native exec: joins the container namespaces via the
    /// tenant builder and captures stdout/stderr through pipes.
    async fn exec_sync_libcontainer(
        &self,
        container_id: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&str>,
        timeout: Option<std::time::Duration>,
    ) -> Result<(i32, Vec<u8>, Vec<u8>)> {
        use libcontainer::container::builder::ContainerBuilder;

        let root = self.lc_root();
        let id = container_id.to_string();
        let cmd_args = args.to_vec();
        let env_map: std::collections::HashMap<String, String> =
            env.iter().cloned().collect();
        let cwd_p = cwd.map(str::to_string);

        tokio::task::spawn_blocking(move || -> Result<(i32, Vec<u8>, Vec<u8>)> {
            let deadline = timeout.map(|d| std::time::Instant::now() + d);
            // Capture through temp files: pipes share the daemon's inherited
            // stdio, and libcontainer intermediate-process tracing can leak
            // into the streamed output. Regular files isolate cleanly and are
            // unreadable to the tenant, unlike /dev/null-swapped fds.
            let out_path = std::env::temp_dir().join(format!("nau-exec-{id}.out"));
            let err_path = std::env::temp_dir().join(format!("nau-exec-{id}.err"));
            let out_file = std::fs::File::create(&out_path)
                .map_err(|e| Error::Runtime(format!("exec out file: {e}")))?;
            let err_file = std::fs::File::create(&err_path)
                .map_err(|e| Error::Runtime(format!("exec err file: {e}")))?;
            let out_owned = std::os::fd::OwnedFd::from(out_file);
            let err_owned = std::os::fd::OwnedFd::from(err_file);

            // Inherit the container's user identity from its config.json so
            // exec runs as the same uid/gid as the container process (CRI
            // exec must honor the container's security context).
            // The OCI spec lives in the container bundle (state.json.bundle);
            // libcontainer's own state dir holds only state.json. Resolve the
            // bundle and read process.user so exec runs as the container uid.
            let (init_uid, init_gid, init_gids, init_apparmor, init_nnp) =
                Self::container_identity(&root, &id);
            eprintln!(
                "EXEC-IDENT id={id} uid={:?} gid={:?} apparmor={:?} nnp={:?}",
                init_uid, init_gid, init_apparmor, init_nnp
            );
            let mut tenant = ContainerBuilder::new(id.clone(), SyscallType::Linux)
                .with_root_path(&root)
                .map_err(|e| Error::Runtime(format!("libcontainer {id}: root: {e}")))?
                .with_stdout(out_owned)
                .with_stderr(err_owned)
                .as_tenant()
                .with_container_args(cmd_args)
                .with_env(env_map);
            if let Some(u) = init_uid {
                tenant = tenant.with_user(Some(u));
            }
            if let Some(g) = init_gid {
                tenant = tenant.with_group(Some(g));
            }
            if !init_gids.is_empty() {
                tenant = tenant.with_additional_gids(init_gids);
            }
            if let Some(a) = &init_apparmor {
                if !a.is_empty() && a != "unconfined" {
                    eprintln!("EXEC-APPARMOR-PROBE applying profile {a:?}");
                    tenant = tenant.with_apparmor(Some(a.clone()));
                }
            }
            if matches!(init_nnp, Some(true)) {
                tenant = tenant.with_no_new_privs(true);
            }
            if let Some(c) = &cwd_p {
                tenant = tenant.with_cwd(Some(std::path::PathBuf::from(c)));
            }
            let (pid, _pty) = tenant
                .build()
                .map_err(|e| Error::Runtime(format!("libcontainer exec {id}: {e}")))?;

            // Raw libc waitpid: libcontainer's Pid belongs to a different
            // nix version than ours. With a CRI timeout, poll without
            // blocking and SIGKILL at the deadline — dropping the future
            // would otherwise leak the sleep-style workload in the
            // container (cri-tools verifies the process is gone).
            let mut st: i32 = 0;
            let raw_pid = pid.as_raw();
            let timed_out = loop {
                let r = unsafe { libc::waitpid(raw_pid, &mut st, libc::WNOHANG) };
                if r < 0 {
                    return Err(Error::Runtime(format!(
                        "exec waitpid: {}",
                        std::io::Error::last_os_error()
                    )));
                }
                if r == raw_pid {
                    break false; // reaped
                }
                if deadline
                    .map(|d| std::time::Instant::now() >= d)
                    .unwrap_or(false)
                {
                    unsafe { libc::kill(raw_pid, libc::SIGKILL) };
                    let _ = unsafe { libc::waitpid(raw_pid, &mut st, 0) };
                    break true;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            if timed_out {
                let _ = std::fs::remove_file(&out_path);
                let _ = std::fs::remove_file(&err_path);
                return Err(Error::Runtime("exec sync timed out".to_string()));
            }
            let code = if libc::WIFEXITED(st) {
                (libc::WEXITSTATUS(st)) as i32
            } else if libc::WIFSIGNALED(st) {
                128 + (libc::WTERMSIG(st)) as i32
            } else {
                -1
            };
            let out_raw = std::fs::read(&out_path)
                .map_err(|e| Error::Runtime(format!("exec read out: {e}")))?;
            let err_raw = std::fs::read(&err_path)
                .map_err(|e| Error::Runtime(format!("exec read err: {e}")))?;
            let _ = std::fs::remove_file(&out_path);
            let _ = std::fs::remove_file(&err_path);
            // The libcontainer intermediate process inherits our process
            // logger and can interleave tracing lines (timestamped) into the
            // captured streams; strip them so CRI exec output stays clean.
            fn strip_log(bytes: Vec<u8>) -> Vec<u8> {
                let Ok(text) = String::from_utf8(bytes.clone()) else { return bytes };
                let lines: Vec<&str> = text.lines().collect();
                let is_log_line = |l: &str| {
                    // Timestamped tracing line, possibly ANSI-wrapped:
                    //   ESC[2m2026-08-26T... or 2026-08-26T...
                    let raw = l.trim_start_matches('\u{1b}').trim_start_matches("[2m");
                    let t = raw.trim_start();
                    let b = t.as_bytes();
                    b.len() >= 20
                        && b[0..4].iter().all(|c| c.is_ascii_digit())
                        && b.get(4) == Some(&b'-')
                        && b.get(7) == Some(&b'-')
                        && b.get(10) == Some(&b'T')
                };
                let mut saw_log_line = false;
                let filtered: Vec<&str> = lines
                    .iter()
                    .filter(|l| {
                        let m = is_log_line(l);
                        if m {
                            saw_log_line = true;
                        }
                        !m
                    })
                    .copied()
                    .collect();
                if !saw_log_line {
                    // Nothing looked like a tracing line; pass through untouched.
                    return bytes;
                }
                if filtered.is_empty() {
                    // Pure log noise: the command itself produced no output.
                    return Vec::new();
                }
                let mut out = filtered.join("\n");
                // Preserve a trailing newline the command emitted.
                if text.ends_with('\n') && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.into_bytes()
            }
            Ok((code, strip_log(out_raw), strip_log(err_raw)))
        })
        .await
        .map_err(|e| Error::Runtime(format!("exec worker: {e}")))?
    }

    /// libcontainer-native streaming exec: joins the container namespaces via
    /// the tenant builder with socketpair stdio so the daemon pumps bytes as
    /// they are produced (exec_sync captures into files, which cannot serve a
    /// live session). Reaped on a WNOHANG poll loop — no deadline, the
    /// session lifetime is bounded by the SPDY connection and the streaming
    /// server SIGKILLs `pid` when the client goes away.
    async fn exec_stream_libcontainer(
        &self,
        container_id: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&str>,
    ) -> Result<crate::sandbox::ExecStreams> {
        use libcontainer::container::builder::ContainerBuilder;

        let root = self.lc_root();
        let id = container_id.to_string();
        let cmd_args = args.to_vec();
        let env_map: std::collections::HashMap<String, String> =
            env.iter().cloned().collect();
        let cwd_p = cwd.map(str::to_string);

        // Pipes (not socketpairs): pipe fds can be re-opened by the exec'd
        // process via /proc/self/fd, which "log to /dev/stderr"-style configs
        // do. Child halves become the tenant's stdio, daemon halves stay async.
        use std::os::fd::FromRawFd;
        let pipe_pair = |what: &str| -> Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
            nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
                .map(|(r, w)| unsafe {
                    (
                        std::os::fd::OwnedFd::from_raw_fd(r),
                        std::os::fd::OwnedFd::from_raw_fd(w),
                    )
                })
                .map_err(|e| Error::Runtime(format!("exec stream {what} pipe: {e}")))
        };
        // stdin: parent writes -> child reads. stdout/stderr: child writes ->
        // parent reads.
        let (in_r, in_w) = pipe_pair("stdin")?;
        let (out_r, out_w) = pipe_pair("stdout")?;
        let (err_r, err_w) = pipe_pair("stderr")?;
        let c_in_fd = in_r;
        let c_out_fd = out_w;
        let c_err_fd = err_w;
        // Blocking reader/writer threads (no tokio I/O driver involvement):
        // pipes registered with the daemon's epoll were observed to interact
        // badly with the fork that creates the tenant process.
        let (in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (err_tx, err_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let id_log = id.clone();
        std::thread::spawn(move || {
            use std::io::Write;
            let mut w = std::fs::File::from(in_w);
            let mut rx = in_rx;
            loop {
                match rx.blocking_recv() {
                    Some(chunk) => {
                        if w.write_all(&chunk).is_err() {
                            tracing::info!(container_id = %id_log, "exec stdin writer: pipe write failed");
                            break;
                        }
                    }
                    None => {
                        // All senders dropped: EOF the process stdin. For
                        // attach the daemon parks a sender for the connection
                        // lifetime, so this only fires on real session end.
                        tracing::info!(container_id = %id_log, "exec stdin writer: channel closed, EOFing process stdin");
                        break;
                    }
                }
            }
        });
        let pump = |label: &'static str,
                    r: std::os::fd::OwnedFd,
                    tx: tokio::sync::mpsc::Sender<Vec<u8>>| {
            let id_log = id.clone();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut r = std::fs::File::from(r);
                let mut buf = [0u8; 8192];
                loop {
                    match r.read(&mut buf) {
                        Ok(0) => {
                            tracing::info!(container_id = %id_log, stream = label, "exec pipe EOF");
                            break;
                        }
                        // SIGCHLD storms from constant forking surface here as
                        // EINTR; dropping the pending buffer truncates CRI
                        // output, so retry.
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            tracing::info!(container_id = %id_log, stream = label, error = %e, "exec pipe read error");
                            break;
                        }
                        Ok(n) => {
                            if tx.blocking_send(buf[..n].to_vec()).is_err() {
                                tracing::info!(container_id = %id_log, stream = label, "exec pump: receiver gone");
                                break;
                            }
                        }
                    }
                }
            });
        };
        pump("stdout", out_r, out_tx);
        pump("stderr", err_r, err_tx);

        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        let pid = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

        let pid_writer = pid.clone();
        tokio::task::spawn_blocking(move || {
            // Spawn failure flows through the same channel as the exit code so
            // the client's error stream carries the reason.
            let spawn = (|| -> std::result::Result<u32, Error> {
                let (init_uid, init_gid, init_gids, init_apparmor, init_nnp) =
                    Self::container_identity(&root, &id);
                let mut tenant = ContainerBuilder::new(id.clone(), SyscallType::Linux)
                    .with_root_path(&root)
                    .map_err(|e| Error::Runtime(format!("libcontainer {id}: root: {e}")))?
                    .with_stdin(c_in_fd)
                    .with_stdout(c_out_fd)
                    .with_stderr(c_err_fd)
                    .as_tenant()
                    .with_container_args(cmd_args)
                    .with_env(env_map);
                if let Some(u) = init_uid {
                    tenant = tenant.with_user(Some(u));
                }
                if let Some(g) = init_gid {
                    tenant = tenant.with_group(Some(g));
                }
                if !init_gids.is_empty() {
                    tenant = tenant.with_additional_gids(init_gids);
                }
                if let Some(a) = &init_apparmor {
                    if !a.is_empty() && a != "unconfined" {
                        tenant = tenant.with_apparmor(Some(a.clone()));
                    }
                }
                if matches!(init_nnp, Some(true)) {
                    tenant = tenant.with_no_new_privs(true);
                }
                if let Some(c) = &cwd_p {
                    tenant = tenant.with_cwd(Some(std::path::PathBuf::from(c)));
                }
                let (pid, _pty) = tenant
                    .build()
                    .map_err(|e| Error::Runtime(format!("libcontainer exec {id}: {e}")))?;
                Ok(pid.as_raw() as u32)
            })();
            let raw_pid = match spawn {
                Ok(p) => {
                    tracing::info!(container_id = %id, pid = p, "streaming exec tenant started");
                    pid_writer.store(p, std::sync::atomic::Ordering::Release);
                    p as i32
                }
                Err(e) => {
                    tracing::info!(container_id = %id, error = %e, "streaming exec tenant spawn failed");
                    let _ = exit_tx.send(Err(e));
                    return;
                }
            };

            let mut st: i32 = 0;
            let outcome = loop {
                let r = unsafe { libc::waitpid(raw_pid, &mut st, libc::WNOHANG) };
                if r < 0 {
                    break Err(Error::Runtime(format!(
                        "exec stream waitpid: {}",
                        std::io::Error::last_os_error()
                    )));
                }
                if r == raw_pid {
                    let code = if libc::WIFEXITED(st) {
                        (libc::WEXITSTATUS(st)) as i32
                    } else if libc::WIFSIGNALED(st) {
                        128 + (libc::WTERMSIG(st)) as i32
                    } else {
                        -1
                    };
                    tracing::info!(container_id = %id, pid = raw_pid, code, "streaming exec tenant exited");
                    break Ok(code);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            let _ = exit_tx.send(outcome);
        });

        Ok(crate::sandbox::ExecStreams {
            stdin: Some(in_tx),
            stdout: out_rx,
            stderr: err_rx,
            pid,
            exit: exit_rx,
        })
    }

    /// runc-CLI exec path (driver = "exec").
    async fn exec_sync_exec(
        &self,
        container_id: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&str>,
        timeout: Option<std::time::Duration>,
    ) -> Result<(i32, Vec<u8>, Vec<u8>)> {
        let mut cmd = Command::new(self.config.runtime.runc.path.clone());
        cmd.arg("exec").arg(container_id).args(args);
        cmd.env_clear();
        cmd.envs(env.iter().cloned());
        cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        if let Some(c) = cwd {
            cmd.arg("-w").arg(c);
        }
        let run = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();
        let out = match timeout {
            Some(t) => tokio::time::timeout(t, run)
                .await
                .map_err(|_| Error::Runtime("exec sync timed out".to_string()))?,
            None => run.await,
        }
        .map_err(|e| Error::Runtime(format!("runc exec {container_id}: {e}")))?;
        Ok((
            out.status.code().unwrap_or(-1),
            out.stdout,
            out.stderr,
        ))
    }
}

impl RuncBackend {
    /// Exec-driver path: shell out to the runc binary.
    async fn start_exec(&self, spec: &WorkloadSpec) -> Result<StartOutcome> {
        let bundle = self.bundle_dir(&spec.id);

        // `runc create` forks the long-lived init; use file-backed stdio.
        let backend = self.clone();
        let id = spec.id.clone();
        tokio::task::spawn_blocking(move || crate::storage::snapshot::with_view(&bundle, || {
            backend.run_runc_lifecycle(&bundle, &["create", "-b", &bundle.to_string_lossy(), &id], "runc create")?;
            backend.run_runc_lifecycle(&bundle, &["start", &id], "runc start")
        })).await.map_err(|e| Error::Runtime(format!("runc view worker: {e}")))??;

        let pid = self
            .state_field(&spec.id, "pid")
            .await?
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        tracing::info!(container_id = %spec.id, pid, "runc container started");
        Ok(StartOutcome::Detached { pid })
    }

    async fn remove_bundle(&self, bundle: PathBuf) -> Result<()> {
        tokio::task::spawn_blocking(move || crate::storage::snapshot::remove(&bundle))
            .await.map_err(|e| Error::Runtime(format!("remove rootfs: {e}")))?
    }

    async fn remove_exec(&self, id: &str) -> Result<()> {
        let mut del = Command::new(self.config.runtime.runc.path.clone());
        del.args(["delete", "-f", id]);
        // --force succeeds for an absent container. Preserve the mount and
        // its pin if deleting a real runtime instance failed.
        self.run_capture(del, &format!("runc delete {id}")).await?;
        Ok(())
    }
}

#[cfg(test)]
mod stop_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    #[test]
    fn exit_race_checks_the_recorded_init_and_preserves_other_errors() {
        let mut container = Container::default();
        let proc_error = LibcontainerError::Procfs(std::io::Error::from(std::io::ErrorKind::NotFound).into());
        container.set_pid(std::process::id() as i32);
        assert!(!init_disappeared(&container, &proc_error).unwrap());
        container.set_pid(i32::MAX);
        assert!(init_disappeared(&container, &proc_error).unwrap());
        assert!(!init_disappeared(&container, &LibcontainerError::NoDirectory).unwrap());

        let tmp = tempfile::tempdir().unwrap();
        assert!(load_lifecycle_container(tmp.path().join("removed")).unwrap().is_none());
        std::fs::write(tmp.path().join("state.json"), b"bad json").unwrap();
        assert!(load_lifecycle_container(tmp.path().into()).is_err());
    }

    #[test]
    fn missing_scope_only_completes_stop_after_its_init_exits() {
        use libcgroups::common::WrappedIoError;
        use libcgroups::systemd::manager::SystemdManagerError;

        let tmp = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let scope = Path::new("/sys/fs/cgroup/system.slice").join(format!("youki-{id}.scope"));
        std::fs::write(
            tmp.path().join("youki_config.json"),
            serde_json::json!({"cgroup_path":format!(":youki:{id}")}).to_string(),
        )
        .unwrap();
        let read_error = |path: &Path, errno| {
            LibcontainerError::CgroupManager(
                SystemdManagerError::WrappedIo(WrappedIoError::Read {
                    path: path.to_owned(),
                    err: std::io::Error::from_raw_os_error(errno),
                })
                .into(),
            )
        };
        let state = |pid| {
            Container::new(&id, OciStatus::Running, Some(pid), tmp.path(), tmp.path()).unwrap()
        };
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 37"])
            .spawn()
            .unwrap();
        let init = state(child.id() as i32);
        let missing = read_error(&scope, libc::ENOENT);
        // Leave the terminating child waitable, as it is while the daemon's
        // workload watcher is still responsible for recording its exit code.
        let mut info = unsafe { std::mem::zeroed() };
        while unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        } < 0
        {
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::Interrupted
            );
        }
        assert!(init_disappeared(&init, &missing).unwrap());
        assert_eq!(child.wait().unwrap().code(), Some(37));
        assert!(init_disappeared(&init, &missing).unwrap());
        assert!(!init_disappeared(&state(std::process::id() as i32), &missing).unwrap());
        assert!(!init_disappeared(&init, &read_error(&scope, libc::EACCES)).unwrap());
        assert!(!init_disappeared(&init, &read_error(&scope.join("child"), libc::ENOENT)).unwrap());
        assert!(!init_disappeared(
            &init,
            &read_error(&tmp.path().join("youki-peer.scope"), libc::ENOENT)
        )
        .unwrap());
        assert!(!init_disappeared(
            &init,
            &read_error(&tmp.path().join(scope.file_name().unwrap()), libc::ENOENT)
        )
        .unwrap());
    }

    #[test]
    fn missing_scope_must_match_the_saved_kubelet_slice_and_container_id() {
        let dir = tempfile::tempdir().unwrap();
        let container =
            Container::new("init", OciStatus::Running, Some(1), dir.path(), dir.path()).unwrap();
        let config = dir.path().join("youki_config.json");
        let slice = "kubepods-burstable-pod123.slice";
        let scope = Path::new("/sys/fs/cgroup/kubepods.slice/kubepods-burstable.slice")
            .join(slice)
            .join("nautilus-init.scope");
        std::fs::write(
            &config,
            serde_json::json!({"cgroup_path":format!("{slice}:nautilus:init")}).to_string(),
        )
        .unwrap();
        assert!(owns_systemd_scope(&container, &scope));
        assert!(!owns_systemd_scope(
            &container,
            Path::new("/sys/fs/cgroup/kubepods-burstable-pod123.slice/nautilus-init.scope")
        ));
        assert!(!owns_systemd_scope(
            &container,
            &scope.with_file_name("nautilus-other.scope")
        ));
        assert!(!owns_systemd_scope(
            &container,
            &scope.join("nautilus-init.scope")
        ));
        std::fs::write(
            &config,
            serde_json::json!({"cgroup_path":format!("{slice}:nautilus:other")}).to_string(),
        )
        .unwrap();
        assert!(!owns_systemd_scope(&container, &scope));
    }

    #[test]
    fn unified_systemd_stats_use_the_owned_scope_for_nested_init() {
        let config = Path::new("kubepods-burstable.slice:nautilus:init");
        let scope = "/kubepods.slice/kubepods-burstable.slice/nautilus-init.scope";
        assert_eq!(
            owned_cgroup_v2("init", config, &format!("0::{scope}/nested\n"), true),
            Some(Path::new("/sys/fs/cgroup").join(scope.trim_start_matches('/')))
        );
        for member in [
            "0::/kubepods-burstable.slice/nautilus-init.scope",
            "0::/kubepods.slice/kubepods-burstable.slice/nautilus-peer.scope",
            "0::/kubepods.slice/kubepods-burstable.slice/nautilus-init.scope/../other",
            "3:cpu:/kubepods.slice/kubepods-burstable.slice/nautilus-init.scope",
            "0::/",
        ] {
            assert_eq!(
                owned_cgroup_v2("init", config, member, true),
                None,
                "{member}"
            );
        }
        let member = "0::/system.slice/youki-init.scope";
        assert_eq!(
            owned_cgroup_v2("init", Path::new(":youki:init"), member, true),
            Some(PathBuf::from(
                "/sys/fs/cgroup/system.slice/youki-init.scope"
            ))
        );
    }

    #[test]
    fn unified_cgroupfs_stats_require_the_saved_container_path() {
        let config = Path::new("/kubepods/pod01/init");
        for member in [
            "2:cpuset:/other\n0::/kubepods/pod01/init\n",
            "0::/kubepods/pod01/init/nested",
        ] {
            assert_eq!(
                owned_cgroup_v2("init", config, member, false),
                Some(PathBuf::from("/sys/fs/cgroup/kubepods/pod01/init"))
            );
        }
        for member in [
            "0::/kubepods/pod01/init-peer",
            "0::/kubepods/pod02/init",
            "0::/kubepods/pod01",
            "0::/kubepods/pod01/init/..",
        ] {
            assert_eq!(
                owned_cgroup_v2("init", config, member, false),
                None,
                "{member}"
            );
        }
        assert_eq!(
            owned_cgroup_v2("peer", config, "0::/kubepods/pod01/init", false),
            None
        );
    }

    #[tokio::test]
    async fn stopping_and_deleting_a_reaped_init_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("test");
        std::fs::create_dir(&root).unwrap();
        let mut init = std::process::Command::new("true").spawn().unwrap();
        let container = Container::new("test", OciStatus::Running, Some(init.id() as i32), tmp.path(), &root).unwrap();
        container.save().unwrap();
        init.wait().unwrap();

        let mut config = Config::default();
        config.runtime.runc.root = tmp.path().into();
        config.storage.container_dir = tmp.path().join("bundles");
        let backend = RuncBackend::new(Arc::new(config));
        for _ in 0..2 {
            backend.stop("test", Duration::ZERO).await.unwrap();
            backend.remove("test").await.unwrap();
        }
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn graceful_and_forced_stop_signals() {
        for (grace, handles_term, expected) in [
            (Duration::from_secs(1), true, "TERM\n"),
            (Duration::from_millis(20), false, "TERM\nKILL\n"),
            (Duration::ZERO, false, "KILL\n"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let runner = tmp.path().join("runc");
            std::fs::write(&runner, r#"#!/bin/sh
cd "${0%/*}"
case "$1" in
    state) printf '{"status":"%s"}\n' "$(cat state)" ;;
    kill) for signal do :; done
          echo "$signal" >> signals
          if [ "$signal" = KILL ] || [ -e handles-term ]; then echo stopped > state; fi ;;
esac
"#).unwrap();
            std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(tmp.path().join("state"), "running").unwrap();
            if handles_term {
                std::fs::write(tmp.path().join("handles-term"), "").unwrap();
            }
            let mut config = Config::default();
            config.runtime.runc.path = runner;
            config.runtime.runc.driver = "exec".into();
            RuncBackend::new(Arc::new(config)).stop("id", grace).await.unwrap();
            assert_eq!(std::fs::read_to_string(tmp.path().join("signals")).unwrap(), expected);
        }
    }
}
