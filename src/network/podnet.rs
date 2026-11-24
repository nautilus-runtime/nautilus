//! Pod-level networking: persistent network namespaces + CNI plugin chains.
//!
//! Each runc-backed pod gets its own network namespace, held alive by a tiny
//! forked holder process (same semantics as `ip netns add`), then wired up by
//! invoking the CNI plugin chain (loopback → bridge by default). The resulting
//! IP surfaces through PodSandboxStatus; containers join the namespace via the
//! OCI spec's `network` namespace path.

use crate::core::{Config, Error, Result};

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Everything needed to tear a pod's network back down.
#[derive(Debug, Clone)]
pub struct PodNetInfo {
    pub netns_path: String,
    pub holder_pid: i32,
    pub ip: String,
}

pub struct PodNet {
    pub(super) config: Arc<Config>,
}

impl PodNet {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }

    fn netns_dir(&self) -> PathBuf {
        PathBuf::from(&self.config.network.netns_dir)
    }

    /// True when CNI plugin binaries exist. Without them we fall back to
    /// host-network pods rather than failing every RunPodSandbox.
    pub fn available(&self) -> bool {
        Path::new(&self.config.network.cni_bin_dir).is_dir()
    }

    /// Set up a standalone sandbox. The CRI service persists the reservation
    /// between these steps so a failed cleanup can also be retried after restart.
    pub async fn setup(&self, pod_id: &str, sysctls: &[(String, String)]) -> Result<Option<PodNetInfo>> {
        let Some(mut info) = self.reserve(pod_id, sysctls).await? else {
            return Ok(None);
        };
        if let Err(error) = self.attach(pod_id, &mut info).await {
            self.teardown(pod_id, &info).await.map_err(|cleanup| {
                Error::Network(format!("{error}; sandbox {pod_id} cleanup failed: {cleanup}"))
            })?;
            return Err(error);
        }
        Ok(Some(info))
    }

    /// Allocate the namespace independently so its teardown handle can be
    /// persisted before even the first CNI plugin allocates resources.
    pub async fn reserve(&self, pod_id: &str, sysctls: &[(String, String)]) -> Result<Option<PodNetInfo>> {
        if !self.available() {
            tracing::warn!(bin_dir = %self.config.network.cni_bin_dir.display(), "CNI unavailable; using host network");
            return Ok(None);
        }
        let handle = self.create_netns(pod_id, sysctls).await?;
        Ok(Some(PodNetInfo {
            netns_path: handle.path.to_string_lossy().into_owned(),
            holder_pid: handle.holder_pid,
            ip: String::new(),
        }))
    }

    pub async fn attach(&self, pod_id: &str, info: &mut PodNetInfo) -> Result<()> {
        let result = self.cni_chain("ADD", pod_id, &info.netns_path, None).await?;
        info.ip = extract_first_ip(&result).unwrap_or_default();
        Ok(())
    }

    pub async fn teardown(&self, pod_id: &str, info: &PodNetInfo) -> Result<()> {
        let path = Path::new(&info.netns_path);
        let netns = if path.try_exists()? { info.netns_path.as_str() } else { "" };
        let ip = (!info.ip.is_empty()).then_some(info.ip.as_str());
        self.cni_chain("DEL", pod_id, netns, ip).await?;
        release_namespace(path, "net", Some(info.holder_pid)).await
    }

    // ---- netns lifecycle -------------------------------------------------

    /// Fork a holder process into a fresh network namespace and publish it at
    /// `<netns_dir>/<pod_id>` via bind-mount (`ip netns add` semantics).
    async fn create_netns(&self, pod_id: &str, sysctls: &[(String, String)]) -> Result<NetnsHandle> {
        let base = self.netns_dir();
        tokio::fs::create_dir_all(&base)
            .await
            .map_err(|e| Error::Network(format!("create {}: {e}", base.display())))?;
        let dst = base.join(pod_id);

        let holder_pid = {
            let dst = dst.clone();
            let sc = sysctls.to_vec();
            tokio::task::spawn_blocking(move || fork_netns_holder(&dst, &sc))
                .await
                .map_err(|e| Error::Network(format!("netns worker join failed: {e}")))??
        };

        Ok(NetnsHandle {
            path: dst,
            holder_pid,
        })
    }

    /// Invoke the CNI plugin chain (ADD or DEL) for one pod.
    ///
    /// Config resolution: `*.conflist`/`*.conf`/`*.json` from
    /// `[network].cni_config_dir`, sorted. When the directory holds no valid
    /// configuration we generate a stock loopback+bridge list using the
    /// configured bridge name/subnet/gateway.
    async fn cni_chain(
        &self,
        command: &str,
        pod_id: &str,
        netns_path: &str,
        prev_ip: Option<&str>,
    ) -> Result<serde_json::Value> {
        let chain = self.load_chain(false)?;
        let prev = prev_ip.map(|ip| serde_json::json!({ "ips": [ { "address": ip } ] }));
        Self::run_chain(&self.config.network.cni_bin_dir, chain, command, pod_id, netns_path, prev).await
    }

    pub(super) async fn run_chain(
        bin_dir: &Path,
        (net_name, cni_version, mut chain): (String, String, Vec<serde_json::Value>),
        command: &str,
        pod_id: &str,
        netns_path: &str,
        mut prev: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let mut attempted = Vec::new();
        let mut delete_error = None;
        if command == "DEL" {
            chain.reverse();
        }
        for plugin in chain {
            let ptype = plugin
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string();
            // loopback configures `lo`; everything else attaches as `eth0`.
            let ifname = if ptype == "loopback" { "lo" } else { "eth0" };
            let mut conf = plugin.clone();
            // In a conflist, `name`/`cniVersion` live at the top level; each
            // plugin invocation still needs them on its own config object.
            if conf.get("name").is_none() {
                conf["name"] = serde_json::json!(net_name);
            }
            if conf.get("cniVersion").is_none() {
                conf["cniVersion"] = serde_json::json!(cni_version);
            }
            if let Some(p) = &prev {
                conf["prevResult"] = p.clone();
            }
            let result = invoke_plugin(
                &bin_dir.join(&ptype),
                &ptype,
                command,
                pod_id,
                netns_path,
                ifname,
                &bin_dir.to_string_lossy(),
                &conf,
            ).await;
            if command == "DEL" {
                if let Err(error) = result {
                    delete_error = Some(error);
                }
                continue;
            }
            // Even the failing plugin may already have allocated resources.
            attempted.push((ptype, ifname, conf));
            match result {
                Ok(value) => {
                    if value.as_object().is_some_and(|object| !object.is_empty()) {
                        prev = Some(value);
                    }
                }
                Err(error) => {
                    for (plugin, interface, config) in attempted.into_iter().rev() {
                        if let Err(cleanup) = invoke_plugin(
                            &bin_dir.join(&plugin), &plugin, "DEL", pod_id, netns_path,
                            interface, &bin_dir.to_string_lossy(), &config,
                        ).await {
                            tracing::warn!(%pod_id, %plugin, %cleanup, "CNI rollback failed");
                        }
                    }
                    return Err(error);
                }
            }
        }
        match delete_error {
            Some(error) => Err(error),
            None => Ok(prev.unwrap_or(serde_json::Value::Null)),
        }
    }

    pub(super) fn load_chain(&self, require_config: bool) -> Result<(String, String, Vec<serde_json::Value>)> {
        use std::fs;

        let dir = PathBuf::from(&self.config.network.cni_config_dir);
        let _ = fs::create_dir_all(&dir);

        let mut files: Vec<PathBuf> = fs::read_dir(&dir)
            .map_err(|e| Error::Network(format!("readdir {}: {e}", dir.display())))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("conf") | Some("json") | Some("conflist")
                )
            })
            .collect();
        files.sort();

        for f in files {
            let config = fs::read_to_string(&f).map_err(Error::from)
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).map_err(Error::from));
            let v = match config {
                Ok(v) => v,
                Err(e) if require_config => return Err(e),
                Err(_) => continue,
            };
            let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("nautilus").to_owned();
            let version = v.get("cniVersion").and_then(|n| n.as_str()).unwrap_or("1.0.0").to_owned();
            let plugins = if let Some(plugins) = v.get("plugins").and_then(|p| p.as_array()) {
                plugins.clone()
            } else {
                vec![v]
            };
            return Ok((name, version, plugins));
        }

        if require_config {
            return Err(Error::Network(format!("no usable VM CNI configuration in {}", dir.display())));
        }

        // No usable configuration: generate the default loopback+bridge list.
        let conf = serde_json::json!({
            "cniVersion": "1.0.0",
            "name": "nautilus",
            "plugins": [
                { "type": "loopback", "cniVersion": "1.0.0" },
                {
                    "type": "bridge",
                    "cniVersion": "1.0.0",
                    "bridge": self.config.network.bridge_name,
                    "isGateway": true,
                    // ipMasq needs the iptables binary; leave NAT wiring to
                    // the operator until we ship a netlink-native masquerade.
                    "ipMasq": false,
                    "ipam": {
                        "type": "host-local",
                        "subnet": self.config.network.subnet,
                        "gateway": self.config.network.gateway
                    }
                }
            ]
        });
        let out = dir.join("10-nautilus.conflist");
        fs::write(&out, serde_json::to_vec_pretty(&conf).unwrap_or_default())
            .map_err(|e| Error::Network(format!("write {}: {e}", out.display())))?;
        Ok((
            "nautilus".to_string(),
            "1.0.0".to_string(),
            conf["plugins"].as_array().cloned().unwrap_or_default(),
        ))
    }
}

struct NetnsHandle {
    path: PathBuf,
    holder_pid: i32,
}

/// Fork a child that joins a new network namespace and parks forever.
/// The parent bind-mounts the child's `/proc/<pid>/ns/net` onto `dst` and then
/// releases the child. Returns the holder pid.
///
/// Fork happens on a dedicated blocking thread; the child never unwinds back
/// into the runtime (it exits directly on any failure), which keeps the
/// fork-in-multithreaded-process hazard contained.
fn fork_netns_holder(dst: &Path, sysctls: &[(String, String)]) -> Result<i32> {
    use nix::sched::{unshare, CloneFlags};
    use nix::unistd::{close, fork, pipe, read, write, ForkResult};
    use std::process::exit;

    let dst = dst.to_path_buf();
    let sysctls = sysctls.to_vec();

    unsafe {
        let (go_r, go_w) = pipe().map_err(|e| Error::Network(format!("pipe: {e}")))?;
        let (rdy_r, rdy_w) = pipe().map_err(|e| Error::Network(format!("pipe: {e}")))?;

        match fork().map_err(|e| Error::Network(format!("fork: {e}")))? {
            ForkResult::Parent { child } => {
                close(rdy_w).ok();
                close(go_r).ok();
                // Wait until the child has actually entered the new netns.
                let mut buf = [0u8; 1];
                read(rdy_r, &mut buf)
                    .map_err(|e| Error::Network(format!("handshake: {e}")))?;
                // mount(2) MS_BIND needs the target to exist (ip netns add
                // touches it too).
                std::fs::File::create(&dst)
                    .map_err(|e| Error::Network(format!("touch {}: {e}", dst.display())))?;
                mount_bind(
                    &PathBuf::from(format!("/proc/{}/ns/net", child.as_raw())),
                    &dst,
                )?;
                write(go_w, &[1]).map_err(|e| Error::Network(format!("release: {e}")))?;
                close(go_w).ok();
                close(rdy_r).ok();
                Ok(child.as_raw())
            }
            ForkResult::Child => {
                close(go_w).ok();
                close(rdy_r).ok();
                if unshare(CloneFlags::CLONE_NEWNET).is_err() {
                    exit(1);
                }
                // Netns-scoped sysctls must be written from inside the new
                // namespace. Apply before the ready handshake so a failure
                // kills the handshake and pod setup fails loudly (fail-closed).
                if !sysctls.is_empty() {
                    use std::path::Path;
                    // Fail-closed: an unapplied sysctl must abort pod setup,
                    // not silently leave the container on defaults.
                    if crate::network::sysctl::apply_sysctls(Path::new("/proc/sys"), &sysctls)
                        .is_err()
                    {
                        exit(1);
                    }
                }
                if write(rdy_w, &[1]).is_err() {
                    exit(1);
                }
                close(rdy_w).ok();
                // Park until released, then hold the namespace open forever.
                let mut buf = [0u8; 1];
                if read(go_r, &mut buf) != Ok(1) {
                    exit(1);
                }
                close(go_r).ok();
                park_namespace_holder()
            }
        }
    }
}

fn mount_bind(src: &Path, dst: &Path) -> Result<()> {
    nix::mount::mount(
        Some(src),
        dst,
        None::<&str>,
        nix::mount::MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| Error::Network(format!("bind-mount {}: {e}", dst.display())))
}

/// Handle path for the pod's shared PID namespace (runc pods).
pub fn pidns_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("/run/nautilus/pidns")
}

/// Create the pod's shared PID namespace: a forked creator unshares
/// CLONE_NEWPID and spawns a parked grandchild as the namespace's init
/// (pid 1 — CRI pause semantics), then exits. The namespace handle is
/// bind-mounted at `/run/nautilus/pidns/<pod_id>` so container specs can
/// join it by path. Returns the parked grandchild's pid for teardown.
pub async fn create_pod_pidns(pod_id: &str) -> Result<i32> {
    let dir = pidns_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| Error::Network(format!("create {}: {e}", dir.display())))?;
    let dst = dir.join(pod_id);
    tokio::task::spawn_blocking(move || fork_pidns_holder(&dst))
        .await
        .map_err(|e| Error::Network(format!("pidns worker join failed: {e}")))?
}

/// Tear down the pod's PID namespace handle and parked init.
pub async fn destroy_pod_pidns(pod_id: &str, pause_pid: Option<i32>) -> Result<()> {
    release_namespace(&pidns_dir().join(pod_id), "pid", pause_pid).await
}

/// Namespace holders never exec, so CLOEXEC cannot release inherited locks or sockets.
fn close_out_inherited_fds() {
    unsafe {
        if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) == 0 {
            return;
        }
        // Older kernels may lack close_range. Avoid allocating or taking libc
        // directory locks after forking the multithreaded daemon.
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            libc::_exit(1);
        }
        let end = limit.rlim_max.min(i32::MAX as libc::rlim_t) as i32;
        for fd in 3..end {
            libc::close(fd);
        }
    }
}

fn park_namespace_holder() -> ! {
    close_out_inherited_fds();
    loop {
        unsafe { libc::pause(); }
    }
}

fn fork_pidns_holder(dst: &Path) -> Result<i32> {
    use nix::sched::{unshare, CloneFlags};
    use nix::unistd::{close, fork, pipe, read, write, ForkResult};
    use std::process::exit;

    let dst = dst.to_path_buf();

    unsafe {
        let (rdy_r, rdy_w) = pipe().map_err(|e| Error::Network(format!("pipe: {e}")))?;
        let (go_r, go_w) = pipe().map_err(|e| Error::Network(format!("pipe: {e}")))?;

        match fork().map_err(|e| Error::Network(format!("fork: {e}")))? {
            // Creator: enters the new PID namespace (future children become
            // its members), spawns the parked init there, reports its pid and
            // exits (reaped by the caller). The namespace survives via the
            // parked grandchild.
            ForkResult::Child => {
                close(go_w).ok();
                close(rdy_r).ok();
                if unshare(CloneFlags::CLONE_NEWPID).is_err() {
                    exit(1);
                }
                // Every arm diverges (parks forever or exits), so this
                // match never yields and the Child arm needs no tail value.
                match fork() {
                    Ok(ForkResult::Child) => {
                        park_namespace_holder()
                    }
                    Ok(ForkResult::Parent { child }) => {
                        let msg = format!("{}\n", child.as_raw());
                        let _ = write(rdy_w, msg.as_bytes());
                        close(rdy_w).ok();
                        let mut buf = [0u8; 1];
                        let _ = read(go_r, &mut buf);
                        exit(0);
                    }
                    Err(_) => exit(1),
                }
            }
            ForkResult::Parent { child } => {
                close(rdy_w).ok();
                close(go_r).ok();
                let mut buf = [0u8; 24];
                let mut got = Vec::new();
                loop {
                    let n = read(rdy_r, &mut buf)
                        .map_err(|e| Error::Network(format!("handshake: {e}")))?;
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                    if got.ends_with(b"\n") {
                        break;
                    }
                }
                close(rdy_r).ok();
                let pause_pid: i32 = std::str::from_utf8(&got)
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                    .ok_or_else(|| Error::Network("pidns handshake: no pid".into()))?;
                std::fs::File::create(&dst)
                    .map_err(|e| Error::Network(format!("touch {}: {e}", dst.display())))?;
                mount_bind(
                    &PathBuf::from(format!("/proc/{}/ns/pid", pause_pid)),
                    &dst,
                )?;
                write(go_w, &[1]).map_err(|e| Error::Network(format!("release: {e}")))?;
                close(go_w).ok();
                // Reap the creator.
                let _ = nix::sys::wait::waitpid(child, None);
                Ok(pause_pid)
            }
        }
    }
}

/// Kill the holder, detach the bind-mount, remove the published path.
/// A retry may encounter a reused pid or a handle already unmounted.
async fn release_namespace(path: &Path, kind: &'static str, holder: Option<i32>) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || release_namespace_blocking(&path, kind, holder)).await
        .map_err(|e| Error::Network(format!("namespace cleanup worker: {e}")))?
}

fn release_namespace_blocking(path: &Path, kind: &str, holder: Option<i32>) -> Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::MetadataExt;
    use nix::{errno::Errno, mount::{umount2, MntFlags}};

    let handle = match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(pid) = holder {
                if Path::new(&format!("/proc/{pid}/ns/{kind}")).try_exists()? {
                    return Err(Error::Network(format!("missing namespace handle {} for live pid {pid}", path.display())));
                }
            }
            return Ok(());
        }
        value => value?,
    };
    if let Some(pid) = holder {
        if pid <= 1 {
            return Err(Error::Network(format!("invalid {kind} namespace holder {pid}")));
        }
        // Bind signals to this incarnation of the pid. Check the namespace
        // identity before sending anything to a surviving process.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        if raw >= 0 {
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            match std::fs::metadata(format!("/proc/{pid}/ns/{kind}")) {
                Ok(ns) if ns.ino() == handle.ino() && ns.dev() == handle.dev() => {
                    let rc = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(),
                        libc::SIGKILL, std::ptr::null::<libc::siginfo_t>(), 0) };
                    if rc < 0 && Errno::last() != Errno::ESRCH {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                    let exited = unsafe { libc::poll(&mut poll, 1, 1000) };
                    if exited < 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    if exited == 0 {
                        return Err(Error::Network(format!("{kind} namespace holder {pid} has not exited")));
                    }
                    // Reap holders forked directly by the daemon; pidns init
                    // and holders inherited from a prior daemon have another parent.
                    let rc = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
                    if rc < 0 && Errno::last() != Errno::ECHILD {
                        return Err(std::io::Error::last_os_error().into());
                    }
                }
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        } else if Errno::last() != Errno::ESRCH {
            return Err(std::io::Error::last_os_error().into());
        }
    }

    match umount2(path, MntFlags::MNT_DETACH) {
        Ok(()) | Err(Errno::EINVAL | Errno::ENOENT) => {}
        Err(e) => return Err(Error::Network(format!("unmount {}: {e}", path.display()))),
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Run one CNI plugin with the standard environment/config contract.
async fn invoke_plugin(
    bin: &Path,
    ptype: &str,
    command: &str,
    pod_id: &str,
    netns_path: &str,
    ifname: &str,
    cni_path: &str,
    conf: &serde_json::Value,
) -> Result<serde_json::Value> {
    use tokio::io::AsyncWriteExt;

    if !bin.exists() {
        return Err(Error::Network(format!(
            "CNI plugin {} missing in bin dir",
            ptype
        )));
    }
    let payload = serde_json::to_vec(conf)
        .map_err(|e| Error::Network(format!("serialize plugin config: {e}")))?;

    let mut cmd = tokio::process::Command::new(bin);
    cmd.env("CNI_COMMAND", command)
        .env("CNI_CONTAINERID", pod_id)
        .env("CNI_NETNS", netns_path)
        .env("CNI_IFNAME", ifname)
        .env("CNI_PATH", cni_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Network(format!("spawn {ptype}: {e}")))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(&payload)
            .await
            .map_err(|e| Error::Network(format!("{ptype} stdin: {e}")))?;
        stdin
            .shutdown()
            .await
            .map_err(|e| Error::Network(format!("{ptype} stdin close: {e}")))?;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| Error::Network(format!("{ptype} wait: {e}")))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(Error::Network(format!(
            "CNI {command} {ptype} failed (rc={:?}): stderr={} stdout={}",
            out.status.code(),
            stderr.trim(),
            stdout.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&stdout)
        .map_err(|e| Error::Network(format!("parse {ptype} result: {e}")))
}

fn extract_first_ip(result: &serde_json::Value) -> Option<String> {
    let addr = result
        .get("ips")?
        .get(0)?
        .get("address")?
        .as_str()?;
    Some(addr.split('/').next().unwrap_or(addr).to_string())
}

/// Directory where per-pod IPC namespace handles are bind-mounted.
pub fn ipcns_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("/run/nautilus/ipcns")
}

/// Fork a child that enters a fresh IPC namespace, applies the IPC-scoped
/// sysctls there and parks forever holding it alive. Mirrors
/// `fork_netns_holder` (same handshake/fail-closed contract).
fn fork_ipcns_holder(dst: &Path, sysctls: &[(String, String)]) -> Result<i32> {
    use nix::sched::{unshare, CloneFlags};
    use nix::unistd::{close, fork, pipe, read, write, ForkResult};
    use std::process::exit;

    let dst = dst.to_path_buf();
    let sysctls = sysctls.to_vec();

    unsafe {
        let (go_r, go_w) = pipe().map_err(|e| Error::Network(format!("pipe: {e}")))?;
        let (rdy_r, rdy_w) = pipe().map_err(|e| Error::Network(format!("pipe: {e}")))?;

        match fork().map_err(|e| Error::Network(format!("fork: {e}")))? {
            ForkResult::Parent { child } => {
                close(rdy_w).ok();
                close(go_r).ok();
                let mut buf = [0u8; 1];
                read(rdy_r, &mut buf)
                    .map_err(|e| Error::Network(format!("handshake: {e}")))?;
                std::fs::File::create(&dst)
                    .map_err(|e| Error::Network(format!("touch {}: {e}", dst.display())))?;
                mount_bind(
                    &PathBuf::from(format!("/proc/{}/ns/ipc", child.as_raw())),
                    &dst,
                )?;
                write(go_w, &[1]).map_err(|e| Error::Network(format!("release: {e}")))?;
                close(go_w).ok();
                close(rdy_r).ok();
                Ok(child.as_raw())
            }
            ForkResult::Child => {
                close(go_w).ok();
                close(rdy_r).ok();
                if unshare(CloneFlags::CLONE_NEWIPC).is_err() {
                    exit(1);
                }
                if !sysctls.is_empty() {
                    if crate::network::sysctl::apply_sysctls(Path::new("/proc/sys"), &sysctls)
                        .is_err()
                    {
                        exit(1);
                    }
                }
                if write(rdy_w, &[1]).is_err() {
                    exit(1);
                }
                close(rdy_w).ok();
                let mut buf = [0u8; 1];
                if read(go_r, &mut buf) != Ok(1) {
                    exit(1);
                }
                close(go_r).ok();
                park_namespace_holder()
            }
        }
    }
}

/// Create the pod's IPC namespace when any IPC-scoped sysctl is requested.
/// Returns the holder pid; the handle lives at `/run/nautilus/ipcns/<pod_id>`
/// so container specs can join it by path with no extra state.
pub async fn create_pod_ipcns(pod_id: &str, sysctls: &[(String, String)]) -> Result<i32> {
    let dir = ipcns_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| Error::Network(format!("create {}: {e}", dir.display())))?;
    let dst = dir.join(pod_id);
    let sc = sysctls.to_vec();
    tokio::task::spawn_blocking(move || fork_ipcns_holder(&dst, &sc))
        .await
        .map_err(|e| Error::Network(format!("ipcns worker join failed: {e}")))?
}

/// Tear down the pod's IPC namespace handle and holder.
pub async fn destroy_pod_ipcns(pod_id: &str, holder_pid: Option<i32>) -> Result<()> {
    release_namespace(&ipcns_dir().join(pod_id), "ipc", holder_pid).await
}

#[cfg(test)]
mod rollback_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn live_holder_does_not_keep_daemon_file_lock() {
        use nix::sys::wait::{waitpid, WaitStatus};
        use nix::unistd::{fork, ForkResult};
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        let file = tempfile::NamedTempFile::new().unwrap();
        let lock = file.reopen().unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
        let (mut parent, holder) = UnixStream::pair().unwrap();
        let child = match unsafe { fork() }.unwrap() {
            ForkResult::Child => unsafe {
                // Keep the test handshake on stdio, just as the holder keeps stdio.
                if libc::dup2(holder.as_raw_fd(), 0) < 0 {
                    libc::_exit(1);
                }
                close_out_inherited_fds();
                let mut byte = 0u8;
                if libc::write(0, (&byte as *const u8).cast(), 1) != 1
                    || libc::read(0, (&mut byte as *mut u8).cast(), 1) != 1
                {
                    libc::_exit(1);
                }
                libc::_exit(0);
            },
            ForkResult::Parent { child } => child,
        };
        drop(holder);
        let ready = parent.read_exact(&mut [0]);
        drop(lock);
        let acquired = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        let _ = parent.write_all(&[1]);
        let status = waitpid(child, None).unwrap();
        ready.unwrap();
        assert_eq!(status, WaitStatus::Exited(child, 0));
        assert_eq!(acquired, 0, "a live holder kept the daemon's file lock");
    }

    #[test]
    fn missing_handle_keeps_a_live_holder_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let error = release_namespace_blocking(&missing, "net", Some(std::process::id() as i32)).unwrap_err();
        assert!(error.to_string().contains("missing namespace handle"));
        release_namespace_blocking(&missing, "net", Some(i32::MAX)).unwrap();
    }

    #[tokio::test]
    async fn failed_cni_add_unwinds_in_reverse() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.network.cni_bin_dir = tmp.path().into();
        config.network.cni_config_dir = tmp.path().into();
        std::fs::write(tmp.path().join("chain.conflist"),
            r#"{"name":"test","plugins":[{"type":"first"},{"type":"second"},{"type":"third"}]}"#).unwrap();
        for plugin in ["first", "second", "third"] {
            let path = tmp.path().join(plugin);
            std::fs::write(&path, r#"#!/bin/sh
cat >/dev/null
echo "$CNI_COMMAND ${0##*/}" >> "$CNI_PATH/calls"
[ "$CNI_COMMAND ${0##*/}" != "ADD second" ]
"#).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let net = PodNet::new(Arc::new(config));
        assert!(net.cni_chain("ADD", "pod", "", None).await.is_err());
        assert_eq!(std::fs::read_to_string(tmp.path().join("calls")).unwrap(),
            "ADD first\nADD second\nDEL second\nDEL first\n");
    }
}
