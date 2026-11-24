//! Run the ignored test on an isolated Linux KVM host with systemd and the
//! lifecycle guest built with guest/build-initramfs.sh. Set NAUTILUS_VM_VMM, NAUTILUS_VM_KERNEL, and
//! NAUTILUS_VM_INITRAMFS to the three asset paths before running
//! cargo test --locked --test vm_pod_lifecycle_test -- --ignored --nocapture.
//! Set NAUTILUS_TEST_DAEMON to the nautilusd executable when running a copied test binary.
#![cfg(target_os = "linux")]

use anyhow::{ensure, Context, Result};
use hyper_util::rt::TokioIo;
use nautilus::core::config::VmConfig;
use nautilus::proto::runtime::v1::{
    runtime_service_client::RuntimeServiceClient, runtime_service_server::RuntimeService,
    ListPodSandboxRequest, PodSandboxConfig, PodSandboxMetadata, PodSandboxState,
    PodSandboxStatusRequest, RemovePodSandboxRequest, RunPodSandboxRequest, StatusRequest,
    StopPodSandboxRequest,
};
use nautilus::vm::owner::PodVmStore;
use nautilus::{
    Config, ContainerManager, NautilusRuntimeService, PodManager, SandboxRegistry, StateManager,
};
use std::fs;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{unix::AsyncFd, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::Request;
use tower::service_fn;
use uuid::Uuid;

fn asset(key: &str) -> Result<PathBuf> {
    let value = std::env::var_os(key).with_context(|| format!("set {key} to the test asset"))?;
    fs::canonicalize(value).with_context(|| format!("resolving {key}"))
}

fn vm_config(root: &Path) -> Result<Arc<Config>> {
    let mut config = Config::default();
    config.runtime.state_dir = root.into();
    config.runtime.runc.root = root.join("runc");
    if let Ok(path) = std::env::var("PATH") {
        if let Some(runc) = std::env::split_paths(&path)
            .map(|p| p.join("runc"))
            .find(|p| p.is_file())
        {
            config.runtime.runc.path = runc;
        }
    }
    config.storage.root_dir = root.join("storage");
    config.storage.image_dir = root.join("images");
    config.storage.container_dir = root.join("containers");
    config.storage.tmp_dir = root.join("tmp");
    config.network.cni_config_dir = root.join("cni");
    config.network.netns_dir = root.join("netns");
    config.cri.listen_address = format!("unix://{}", root.join("cri.sock").display());
    config.runtime.sandboxes.vm = Some(VmConfig {
        vmm: asset("NAUTILUS_VM_VMM")?,
        kernel: asset("NAUTILUS_VM_KERNEL")?,
        initramfs: asset("NAUTILUS_VM_INITRAMFS")?,
        virtiofsd: std::env::var_os("NAUTILUS_VM_VIRTIOFSD").map(PathBuf::from),
    });
    Ok(Arc::new(config))
}

async fn create_attempt(pods: &PodManager, name: &str) -> Result<String> {
    Ok(pods
        .create_pod(
            "vm-test",
            name,
            &Uuid::new_v4().to_string(),
            "vm",
            Default::default(),
            Default::default(),
            "",
            "",
            &[],
            &[],
            &[],
            false,
            false,
            false,
            false,
            None,
        )
        .await?)
}

// Seed the experimental VM attempts through PodManager before the daemon opens sled.
// The public vm handler is gated until guest OCI workloads and networking are available.
async fn boot_pair(config: Arc<Config>, root: &Path, attempts: &mut Vec<String>) -> Result<()> {
    let state = Arc::new(StateManager::new(root.join("state.db"))?);
    let registry = Arc::new(SandboxRegistry::new(&config)?);
    let containers = Arc::new(ContainerManager::new(
        config.clone(),
        state.clone(),
        registry,
    ));
    let pods = PodManager::new(config, state, containers);
    attempts.push(create_attempt(&pods, "a").await?);
    attempts.push(create_attempt(&pods, "b").await?);
    let (a, b) = tokio::join!(pods.start_pod(&attempts[0]), pods.start_pod(&attempts[1]));
    a.context("starting Pod A")?;
    b.context("starting Pod B")?;
    Ok(())
}

type Client = RuntimeServiceClient<Channel>;

fn cri_client(root: &Path) -> Client {
    let path = root.join("cri.sock");
    let endpoint = Endpoint::from_static("http://nautilus").timeout(Duration::from_secs(30));
    let channel = endpoint.connect_with_connector_lazy(service_fn(move |_: Uri| {
        let path = path.clone();
        async move { UnixStream::connect(path).await.map(TokioIo::new) }
    }));
    Client::new(channel)
}

async fn start_daemon(root: &Path) -> Result<(Child, Client)> {
    let binary = std::env::var_os("NAUTILUS_TEST_DAEMON")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_nautilusd").into());
    // nautilusd initializes a bridge and NAT rules at startup. Isolate them
    // for each daemon. unshare execs nautilusd in place, preserving its PID.
    let mut daemon = Command::new("unshare")
        .args(["--net", "--"])
        .arg(binary)
        .arg("--config")
        .arg(root.join("config.toml"))
        .kill_on_drop(true)
        .spawn()
        .context("launching nautilusd with a private network namespace")?;
    let mut cri = cri_client(root);
    timeout(Duration::from_secs(45), async {
        loop {
            if let Some(exit) = daemon.try_wait()? {
                anyhow::bail!("nautilusd exited during startup: {exit}");
            }
            if cri.status(StatusRequest::default()).await.is_ok() {
                return Ok::<(), anyhow::Error>(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("waiting for nautilusd CRI socket")??;
    Ok((daemon, cri))
}

async fn sandbox_guest(cri: &mut Client, id: &str) -> Result<Option<String>> {
    let response = cri
        .pod_sandbox_status(PodSandboxStatusRequest {
            pod_sandbox_id: id.into(),
            verbose: true,
        })
        .await?
        .into_inner();
    let pod = response.status.context("missing PodSandboxStatus")?;
    ensure!(
        pod.id == id && pod.runtime_handler == "vm",
        "wrong sandbox in CRI status"
    );
    ensure!(
        pod.state() == PodSandboxState::SandboxNotready,
        "incomplete VM became Ready"
    );
    let details: serde_json::Value = serde_json::from_str(
        response
            .info
            .get("nautilus.vm")
            .context("missing guest identity details")?,
    )?;
    ensure!(
        details.get("error").is_none(),
        "VM status failed: {details}"
    );
    match details["guestReachable"].as_bool() {
        Some(true) => Ok(Some(
            details["bootId"]
                .as_str()
                .context("missing boot ID")?
                .to_owned(),
        )),
        Some(false) => Ok(None),
        None => anyhow::bail!("missing guest reachability: {details}"),
    }
}

async fn stop_sandbox(cri: &mut Client, id: &str) -> Result<()> {
    cri.stop_pod_sandbox(StopPodSandboxRequest {
        pod_sandbox_id: id.into(),
    })
    .await?;
    Ok(())
}

async fn remove_sandbox(cri: &mut Client, id: &str) -> Result<()> {
    cri.remove_pod_sandbox(RemovePodSandboxRequest {
        pod_sandbox_id: id.into(),
    })
    .await?;
    Ok(())
}

// Pin the process that owns this private API socket, even if its PID is later reused.
async fn vmm_process(api: &Path) -> Result<AsyncFd<OwnedFd>> {
    let peer = UnixStream::connect(api).await?;
    let pid = peer
        .peer_cred()?
        .pid()
        .context("VMM socket has no peer PID")?;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) } as i32;
    if fd == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    // pidfd_open returns a new owned descriptor.
    Ok(AsyncFd::new(unsafe { OwnedFd::from_raw_fd(fd) })?)
}

async fn expect_vmm_exit(process: &AsyncFd<OwnedFd>) -> Result<()> {
    drop(
        timeout(Duration::from_secs(5), process.readable())
            .await
            .context("VMM process remains alive")??,
    );
    Ok(())
}

async fn check_share(root: &Path, a: &str, b: &str, boot_a: &str, boot_b: &str) -> Result<()> {
    let store = PodVmStore::new(root.join("vms"));
    let pods = [
        (a, b, boot_a, Uuid::new_v4()),
        (b, a, boot_b, Uuid::new_v4()),
    ];
    for (id, other, _, marker) in &pods {
        let staged = tempfile::tempdir_in(root)?;
        fs::write(staged.path().join("marker"), format!("{other} {marker}"))?;
        store.publish_bundle(id, id, staged.path()).await?;
    }
    for (id, _, boot, marker) in &pods {
        let exported = root.join("vms").join(id).join("podfs/containers").join(id);
        let expected = format!("{boot} {marker}\n");
        timeout(Duration::from_secs(20), async {
            while fs::read_to_string(exported.join("observed")).ok().as_ref() != Some(&expected) {
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("guest did not confirm isolation and write back its marker")?;
        let replacement = tempfile::tempdir_in(root)?;
        ensure!(store
            .publish_bundle(id, id, replacement.path())
            .await
            .is_err());
        ensure!(replacement.path().is_dir());
        eprintln!("guest {boot} saw only its own bundle, wrote back marker {marker}");
    }

    // An unexpected same-filesystem bind mount is a separate mount even if
    // st_dev is unchanged. Both publication and cleanup must retain its data.
    let host = tempfile::tempdir_in(root)?;
    fs::write(host.path().join("preserved"), "host data")?;
    let clean = tempfile::tempdir_in(root)?;
    let staged_mount = clean.path().join("volume");
    fs::create_dir(&staged_mount)?;
    let bind = |where_: &Path| -> Result<()> {
        nix::mount::mount(
            Some(host.path()),
            where_,
            None::<&str>,
            nix::mount::MsFlags::MS_BIND,
            None::<&str>,
        )?;
        Ok(())
    };
    bind(&staged_mount)?;
    let publish = store
        .publish_bundle(a, &Uuid::new_v4().to_string(), clean.path())
        .await;
    nix::mount::umount(&staged_mount)?;
    ensure!(publish.is_err() && clean.path().is_dir());
    let in_bundle = root
        .join("vms")
        .join(a)
        .join("podfs/containers")
        .join(a)
        .join("volume");
    fs::create_dir(&in_bundle)?;
    bind(&in_bundle)?;
    let remove = store.remove_bundle(a, a).await;
    nix::mount::umount(&in_bundle)?;
    ensure!(remove.is_err() && fs::read_to_string(host.path().join("preserved"))? == "host data");

    // A replaced export component invalidates this Pod. The failed operation
    // must leave the staged source and the replacement directory untouched.
    let containers = root.join("vms").join(a).join("podfs/containers");
    let saved = containers.with_file_name("saved");
    let outside = tempfile::tempdir_in(root)?;
    fs::rename(&containers, &saved)?;
    std::os::unix::fs::symlink(outside.path(), &containers)?;
    let staged = tempfile::tempdir_in(root)?;
    ensure!(store.publish_bundle(a, a, staged.path()).await.is_err());
    ensure!(store.remove_bundle(a, a).await.is_err());
    ensure!(store.status(a).await.is_err());
    ensure!(outside.path().read_dir()?.count() == 0 && staged.path().is_dir());
    fs::remove_file(&containers)?;
    fs::rename(saved, &containers)?;
    for (id, _, _, _) in pods {
        for _ in 0..2 {
            store.remove_bundle(id, id).await?;
        }
        ensure!(!root
            .join("vms")
            .join(id)
            .join("podfs/containers")
            .join(id)
            .exists());
    }
    Ok(())
}

async fn lose_share(id: &str) -> Result<()> {
    let units = Command::new("systemctl")
        .args([
            "list-units",
            "--state=active",
            "--type=service",
            "--no-legend",
            "--plain",
            &format!("nautilus-virtiofs-{id}-*.service"),
        ])
        .output()
        .await?;
    ensure!(units.status.success());
    let lines = String::from_utf8(units.stdout)?;
    let names: Vec<_> = lines
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    ensure!(names.len() == 1 && names[0].starts_with(&format!("nautilus-virtiofs-{id}-")));
    ensure!(Command::new("systemctl")
        .args(["kill", "--signal=KILL", names[0]])
        .status()
        .await?
        .success());
    Ok(())
}

async fn check_recovery(root: &Path, a: &str, b: &str) -> Result<()> {
    let (mut first, mut cri) = start_daemon(root).await?;
    let boot_a = sandbox_guest(&mut cri, a).await?.context("A unavailable")?;
    let boot_b = sandbox_guest(&mut cri, b).await?.context("B unavailable")?;
    ensure!(
        Uuid::parse_str(&boot_a)? != Uuid::parse_str(&boot_b)?,
        "shared VM identity"
    );
    let vms = root.join("vms");
    for id in [a, b] {
        for endpoint in ["api.sock", "vsock.sock"] {
            ensure!(fs::symlink_metadata(vms.join(id).join(endpoint))?
                .file_type()
                .is_socket());
        }
    }
    let process_a = vmm_process(&vms.join(a).join("api.sock")).await?;
    let process_b = vmm_process(&vms.join(b).join("api.sock")).await?;

    first.start_kill()?;
    ensure!(
        first.wait().await?.signal() == Some(libc::SIGKILL),
        "nautilusd survived SIGKILL"
    );
    drop(cri);
    let (mut second, mut cri) = start_daemon(root).await?;
    ensure!(
        sandbox_guest(&mut cri, a).await?.as_ref() == Some(&boot_a),
        "lost A after daemon restart"
    );
    ensure!(
        sandbox_guest(&mut cri, b).await?.as_ref() == Some(&boot_b),
        "lost B after daemon restart"
    );

    if std::env::var_os("NAUTILUS_VM_VIRTIOFSD").is_some() {
        check_share(root, a, b, &boot_a, &boot_b).await?;
        lose_share(a).await?;
        timeout(Duration::from_secs(8), async {
            while !matches!(sandbox_guest(&mut cri, a).await, Ok(None)) {
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("CRI did not observe A's lost virtiofsd")?;
        ensure!(sandbox_guest(&mut cri, b).await?.as_deref() == Some(boot_b.as_str()));
    }
    if std::env::var_os("NAUTILUS_VM_VIRTIOFSD").is_some() {
        let containers = vms.join(a).join("podfs/containers");
        let saved = containers.with_file_name("original-containers");
        fs::rename(&containers, &saved)?;
        fs::create_dir(&containers)?;
        let rejected = stop_sandbox(&mut cri, a).await;
        ensure!(rejected.is_err() && vms.join(format!("{a}.json")).is_file());
        fs::remove_dir(&containers)?;
        fs::rename(saved, &containers)?;
        ensure!(sandbox_guest(&mut cri, b).await?.as_deref() == Some(boot_b.as_str()));
    }

    let mut retry = cri_client(root);
    tokio::try_join!(stop_sandbox(&mut cri, a), stop_sandbox(&mut retry, a))?;
    expect_vmm_exit(&process_a).await?;
    ensure!(
        sandbox_guest(&mut cri, a).await?.is_none(),
        "A survived StopPodSandbox"
    );
    tokio::try_join!(remove_sandbox(&mut cri, a), stop_sandbox(&mut retry, a))?;
    remove_sandbox(&mut cri, a).await?;
    ensure!(
        !vms.join(a).exists() && !vms.join(format!("{a}.json")).exists(),
        "A resources remain"
    );
    ensure!(
        sandbox_guest(&mut cri, b).await?.as_ref() == Some(&boot_b),
        "stopping A disrupted B"
    );

    // Lose B's VMM independently of both daemon processes.
    let mut api = UnixStream::connect(vms.join(b).join("api.sock")).await?;
    api.write_all(
        b"PUT /api/v1/vmm.shutdown HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    )
    .await?;
    expect_vmm_exit(&process_b).await?;
    timeout(Duration::from_secs(15), async {
        loop {
            if matches!(sandbox_guest(&mut cri, b).await, Ok(None)) {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("CRI did not observe B's VMM exit")?;
    for _ in 0..2 {
        remove_sandbox(&mut cri, b).await?;
    }
    ensure!(
        !vms.join(b).exists() && !vms.join(format!("{b}.json")).exists(),
        "B resources remain"
    );
    ensure!(cri
        .list_pod_sandbox(ListPodSandboxRequest::default())
        .await?
        .into_inner()
        .items
        .is_empty());
    eprintln!("CRI reconnected after SIGKILL; guest boot IDs A={boot_a} B={boot_b}; both VMMs and records removed");
    second.kill().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires systemd, /dev/kvm, Cloud Hypervisor, Nautilus guest assets and root for network namespaces"]
async fn two_pods_survive_owner_restart() -> Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("nt-vm-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")?;
    let root = dir.path();
    let config = vm_config(root)?;
    config.to_file(root.join("config.toml"))?;
    let mut attempts = Vec::new();
    let result = async {
        boot_pair(config, root, &mut attempts).await?;
        check_recovery(root, &attempts[0], &attempts[1]).await
    }
    .await;
    // Retain the private records for diagnosis if cleanup cannot confirm ownership.
    let store = PodVmStore::new(root.join("vms"));
    let mut errors = Vec::new();
    for id in &attempts {
        if let Err(error) = store.remove(id).await {
            errors.push(format!("{id}: {error}"));
        }
    }
    if !errors.is_empty() {
        let retained = dir.into_path();
        anyhow::bail!(
            "VM cleanup failed; retained {}: {errors:?}; scenario: {result:?}",
            retained.display()
        );
    }
    result
}

#[tokio::test]
async fn default_cri_service_does_not_offer_or_accept_vm() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = Config::default();
    config.runtime.state_dir = dir.path().into();
    let state = Arc::new(StateManager::new(dir.path().join("state.db"))?);
    let service = NautilusRuntimeService::new(Arc::new(config), state.clone());
    let status = service
        .status(Request::new(StatusRequest::default()))
        .await?
        .into_inner();
    ensure!(status
        .runtime_handlers
        .iter()
        .all(|handler| handler.name != "vm"));
    let result = service
        .run_pod_sandbox(Request::new(RunPodSandboxRequest {
            runtime_handler: "vm".into(),
            config: Some(PodSandboxConfig {
                metadata: Some(PodSandboxMetadata {
                    name: "unconfigured".into(),
                    uid: Uuid::new_v4().to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }))
        .await;
    let error = result.expect_err("unconfigured service accepted vm");
    ensure!(
        error.message().contains("vm"),
        "unexpected rejection: {error}"
    );
    ensure!(
        state.list_pods().await?.is_empty(),
        "rejected request allocated a Pod"
    );
    Ok(())
}

#[tokio::test]
async fn a_persisted_vm_attempt_without_an_owner_is_failed_and_can_be_removed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = Config::default();
    config.runtime.state_dir = dir.path().into();
    let config = Arc::new(config);
    let db = dir.path().join("state.db");
    let id = {
        let state = Arc::new(StateManager::new(&db)?);
        let registry = Arc::new(SandboxRegistry::new(&config)?);
        let containers = Arc::new(ContainerManager::new(
            config.clone(),
            state.clone(),
            registry,
        ));
        create_attempt(
            &PodManager::new(config.clone(), state, containers),
            "abandoned",
        )
        .await?
    };
    // Recovery must still handle this VM when an unrelated Pod cannot be decoded.
    {
        let raw = sled::open(&db)?;
        raw.insert(b"pod:broken", b"{".as_slice())?;
        raw.flush()?;
    }
    let state = Arc::new(StateManager::new(&db)?);
    let service = NautilusRuntimeService::new(config, state.clone());
    service.recover_vm_pods().await?;
    ensure!(
        state
            .get_pod_state(&id)
            .await?
            .context("lost vm attempt")?
            .status
            == nautilus::core::PodStatus::Failed
    );
    let status = service
        .pod_sandbox_status(Request::new(PodSandboxStatusRequest {
            pod_sandbox_id: id.clone(),
            verbose: true,
        }))
        .await?
        .into_inner();
    ensure!(status.status.context("missing sandbox")?.state() == PodSandboxState::SandboxNotready);
    let vm: serde_json::Value = serde_json::from_str(
        status
            .info
            .get("nautilus.vm")
            .context("missing vm status")?,
    )?;
    ensure!(vm["guestReachable"] == false);
    for _ in 0..2 {
        service
            .stop_pod_sandbox(Request::new(StopPodSandboxRequest {
                pod_sandbox_id: id.clone(),
            }))
            .await?;
    }
    for _ in 0..2 {
        service
            .remove_pod_sandbox(Request::new(RemovePodSandboxRequest {
                pod_sandbox_id: id.clone(),
            }))
            .await?;
    }
    ensure!(state.get_pod_state(&id).await?.is_none());
    ensure!(
        !dir.path().join("vms").exists(),
        "cleanup materialized missing VM owner directory"
    );
    Ok(())
}

#[tokio::test]
async fn terminal_vm_start_failure_is_recorded_immediately() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = Config::default();
    config.runtime.state_dir = dir.path().into();
    config.runtime.sandboxes.vm = Some(VmConfig {
        vmm: dir.path().join("missing-vmm"),
        kernel: dir.path().join("kernel"),
        initramfs: dir.path().join("agent"),
        virtiofsd: None,
    });
    let config = Arc::new(config);
    let state = Arc::new(StateManager::new(dir.path().join("state.db"))?);
    let registry = Arc::new(SandboxRegistry::new(&config)?);
    let containers = Arc::new(ContainerManager::new(
        config.clone(),
        state.clone(),
        registry,
    ));
    let pods = PodManager::new(config, state.clone(), containers);
    let id = create_attempt(&pods, "failed-start").await?;
    ensure!(pods.start_pod(&id).await.is_err());
    ensure!(pods.get_pod_status(&id).await? == Some(nautilus::core::PodStatus::Failed));
    ensure!(
        state
            .get_pod_state(&id)
            .await?
            .context("lost failed attempt")?
            .status
            == nautilus::core::PodStatus::Failed
    );
    Ok(())
}

#[tokio::test]
async fn missing_pod_cannot_take_ownership_of_a_staged_bundle() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let staged = dir.path().join("bundle");
    fs::create_dir(&staged)?;
    let store = PodVmStore::new(dir.path().join("vms"));
    let pod = Uuid::new_v4().to_string();
    let container = Uuid::new_v4().to_string();
    ensure!(store
        .publish_bundle(&pod, &container, &staged)
        .await
        .is_err());
    ensure!(staged.is_dir() && !dir.path().join("vms").exists());
    store.remove_bundle(&pod, &container).await?;
    ensure!(store.remove_bundle(&pod, "../elsewhere").await.is_err());
    Ok(())
}

#[tokio::test]
#[ignore = "requires root and systemd, KVM, VMM and virtiofsd assets"]
async fn failed_filesystem_or_vmm_start_releases_the_complete_attempt() -> Result<()> {
    let work = tempfile::tempdir()?;
    let store = PodVmStore::new(work.path().join("vms"));
    let mut config = vm_config(work.path())?
        .runtime
        .sandboxes
        .vm
        .clone()
        .unwrap();
    let backend = asset("NAUTILUS_VM_VIRTIOFSD")?;
    let vmm = config.vmm.clone();
    for (executable, daemon) in [
        (vmm.clone(), work.path().join("missing-virtiofsd")),
        (vmm, PathBuf::from("/bin/false")),
        (PathBuf::from("/bin/false"), backend),
    ] {
        config.vmm = executable;
        config.virtiofsd = Some(daemon);
        let id = Uuid::new_v4().to_string();
        let error = store
            .start_configured(&id, &config)
            .await
            .expect_err("unexpectedly booted");
        eprintln!("rolled back {id}: {error}");
        ensure!(store.status(&id).await?.is_none());
        ensure!(store.recover(&id).await?.is_none());
        store.remove(&id).await?;
        ensure!(!work.path().join("vms").join(&id).exists());
        let units = Command::new("systemctl")
            .args([
                "list-units",
                "--all",
                "--no-legend",
                "--plain",
                &format!("nautilus-*-{id}-*.service"),
            ])
            .output()
            .await?;
        ensure!(units.status.success() && units.stdout.is_empty());
    }
    Ok(())
}
