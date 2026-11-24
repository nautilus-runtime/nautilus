//! Run on an isolated root/systemd/KVM Linux host with a workload initramfs:
//! NAUTILUS_VM_{VMM,KERNEL,INITRAMFS,VIRTIOFSD}, NAUTILUS_CNI_BIN_DIR and
//! NAUTILUS_TEST_BUSYBOX (static). NAUTILUS_TEST_DAEMON selects a copied daemon.
//! cargo test --locked --test vm_cri_workloads_test -- --ignored --nocapture
#![cfg(target_os = "linux")]

use anyhow::{bail, ensure, Context, Result};
use hyper_util::rt::TokioIo;
use nautilus::proto::runtime::v1::{
    image_service_client::ImageServiceClient, runtime_service_client::RuntimeServiceClient, *,
};
use nautilus::Config;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;
use uuid::Uuid;

type Runtime = RuntimeServiceClient<Channel>;

fn asset(key: &str) -> Result<PathBuf> {
    fs::canonicalize(std::env::var_os(key).with_context(|| format!("set {key}"))?)
        .with_context(|| format!("resolve {key}"))
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

// Generate one immutable OCI manifest and the content-addressed blob a previous
// native pull would have left on this node. PullImage by digest then exercises
// the normal offline image path. The registry address is loopback only.
fn cache_image(images: &Path) -> Result<String> {
    let mut tar = tar::Builder::new(Vec::new());
    for path in ["bin", "dev", "etc", "proc", "workspace"] {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_size(0);
        h.set_mode(0o755);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        tar.append_data(&mut h, path, &[][..])?;
    }
    let bb = fs::read(asset("NAUTILUS_TEST_BUSYBOX")?)?;
    let mut h = tar::Header::new_gnu();
    h.set_size(bb.len() as u64);
    h.set_mode(0o755);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    tar.append_data(&mut h, "bin/busybox", bb.as_slice())?;
    let blob = tar.into_inner()?;
    let layer_digest = digest(&blob);
    let arch = if std::env::consts::ARCH == "aarch64" {
        "arm64"
    } else {
        "amd64"
    };
    let cfg = json!({"architecture":arch,"os":"linux","rootfs":{"type":"layers","diff_ids":[layer_digest]},
        "config":{"WorkingDir":"/workspace","Env":["PATH=/bin","IMAGE_VAR=oci-cache"],"Entrypoint":["/bin/busybox"]}});
    let bytes = serde_json::to_vec(&cfg)?;
    let manifest = json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json",
        "config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":digest(&bytes),"size":bytes.len()},
        "layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":layer_digest,"size":blob.len()}]});
    let resolved = digest(&serde_json::to_vec(&manifest)?);
    let image = format!("127.0.0.1:1/test/guest@{resolved}");
    fs::create_dir_all(images.join("layers"))?;
    fs::create_dir_all(images.join("manifests"))?;
    fs::write(
        images
            .join("layers")
            .join(format!("{}.blob", layer_digest.replace(':', "-"))),
        blob,
    )?;
    let file = image.replace(['/', ':', '@'], "_");
    fs::write(
        images.join("manifests").join(format!("{file}.json")),
        serde_json::to_vec(&json!({"manifest":manifest,"config":cfg,"manifest_digest":resolved}))?,
    )?;
    Ok(image)
}

fn configure(root: &Path) -> Result<String> {
    let mut config = Config::default();
    config.runtime.state_dir = root.join("state");
    config.runtime.runc.root = root.join("runc");
    if std::env::var_os("NAUTILUS_TEST_RUNC").is_some() {
        config.runtime.runc.path = asset("NAUTILUS_TEST_RUNC")?;
    } else if let Some(path) = ["/usr/sbin/runc", "/usr/bin/runc", "/usr/local/sbin/runc"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
    {
        config.runtime.runc.path = path;
    }
    config.storage.root_dir = root.join("storage");
    config.storage.image_dir = root.join("images");
    config.storage.container_dir = root.join("containers");
    config.storage.tmp_dir = root.join("tmp");
    config.storage.registry_mirror.clear();
    config.network.cni_config_dir = root.join("cni");
    config.network.cni_bin_dir = asset("NAUTILUS_CNI_BIN_DIR")?;
    config.network.netns_dir = root.join("netns");
    config.network.bridge_name = "ntcriw15".into();
    config.network.subnet = "10.231.15.0/24".into();
    config.cri.listen_address = format!("unix://{}", root.join("cri.sock").display());
    let image = cache_image(&config.storage.image_dir)?;
    fs::create_dir_all(&config.network.cni_config_dir)?;
    let chain = json!({"cniVersion":"0.4.0","name":"ntcriw15","plugins":[{"type":"loopback"},
        {"type":"bridge","bridge":"ntcriw15","isGateway":true,"ipam":{"type":"host-local",
        "subnet":"10.231.15.0/24","routes":[{"dst":"0.0.0.0/0"}],"dataDir":root.join("ipam")},
        "dns":{"nameservers":["10.231.15.1"]}}]});
    fs::write(
        config.network.cni_config_dir.join("workload.conflist"),
        chain.to_string(),
    )?;
    let mut raw = toml::Value::try_from(&config)?;
    let sandboxes = raw
        .get_mut("runtime")
        .and_then(|r| r.get_mut("sandboxes"))
        .and_then(|v| v.as_table_mut())
        .context("missing runtime.sandboxes")?;
    sandboxes.insert("vm_workloads_enabled".into(), true.into());
    let mut vm = toml::Table::new();
    for (field, key) in [
        ("vmm", "NAUTILUS_VM_VMM"),
        ("kernel", "NAUTILUS_VM_KERNEL"),
        ("initramfs", "NAUTILUS_VM_INITRAMFS"),
        ("virtiofsd", "NAUTILUS_VM_VIRTIOFSD"),
    ] {
        vm.insert(
            field.into(),
            asset(key)?.to_string_lossy().into_owned().into(),
        );
    }
    sandboxes.insert("vm".into(), vm.into());
    fs::write(root.join("config.toml"), toml::to_string(&raw)?)?;
    Ok(image)
}

fn channel(root: &Path) -> Channel {
    let sock = root.join("cri.sock");
    Endpoint::from_static("http://nautilus")
        .timeout(Duration::from_secs(60))
        .connect_with_connector_lazy(service_fn(move |_: Uri| {
            let path = sock.clone();
            async move { UnixStream::connect(path).await.map(TokioIo::new) }
        }))
}

async fn daemon(root: &Path) -> Result<(Child, Runtime)> {
    let bin = std::env::var_os("NAUTILUS_TEST_DAEMON")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_nautilusd").into());
    let mut child = Command::new(bin)
        .arg("--config")
        .arg(root.join("config.toml"))
        .kill_on_drop(true)
        .spawn()?;
    let mut cli = Runtime::new(channel(root));
    timeout(Duration::from_secs(30), async {
        loop {
            if let Some(status) = child.try_wait()? {
                bail!("daemon startup failed: {status}")
            }
            if cli.status(StatusRequest::default()).await.is_ok() {
                return Ok::<_, anyhow::Error>(());
            }
            sleep(Duration::from_millis(80)).await;
        }
    })
    .await
    .context("starting test daemon")??;
    Ok((child, cli))
}

fn namespaces() -> NamespaceOption {
    NamespaceOption {
        network: NamespaceMode::Pod.into(),
        pid: NamespaceMode::Container.into(),
        ipc: NamespaceMode::Pod.into(),
        ..Default::default()
    }
}

fn pod_config(root: &Path, name: &str) -> PodSandboxConfig {
    PodSandboxConfig {
        metadata: Some(PodSandboxMetadata {
            name: name.into(),
            uid: Uuid::new_v4().to_string(),
            namespace: "vm-test".into(),
            ..Default::default()
        }),
        log_directory: root.join(name).to_string_lossy().into_owned(),
        linux: Some(LinuxPodSandboxConfig {
            security_context: Some(LinuxSandboxSecurityContext {
                namespace_options: Some(namespaces()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn boot(cli: &mut Runtime, config: &PodSandboxConfig) -> Result<String> {
    fs::create_dir_all(&config.log_directory)?;
    Ok(cli
        .run_pod_sandbox(RunPodSandboxRequest {
            config: Some(config.clone()),
            runtime_handler: "vm".into(),
        })
        .await?
        .into_inner()
        .pod_sandbox_id)
}

async fn boot_id(cli: &mut Runtime, id: &str) -> Result<String> {
    let res = cli
        .pod_sandbox_status(PodSandboxStatusRequest {
            pod_sandbox_id: id.into(),
            verbose: true,
        })
        .await?
        .into_inner();
    let p = res.status.context("missing Pod status")?;
    ensure!(
        p.state() == PodSandboxState::SandboxReady && p.runtime_handler == "vm",
        "Pod not ready: {p:?}"
    );
    let vm: serde_json::Value =
        serde_json::from_str(res.info.get("nautilus.vm").context("missing identity")?)?;
    Ok(Uuid::parse_str(vm["bootId"].as_str().context("missing boot ID")?)?.to_string())
}

fn workload(name: &str, image: &str, program: &str) -> ContainerConfig {
    ContainerConfig {
        metadata: Some(ContainerMetadata {
            name: name.into(),
            attempt: 0,
        }),
        image: Some(ImageSpec {
            image: image.into(),
            ..Default::default()
        }),
        command: vec![
            "/bin/busybox".into(),
            "sh".into(),
            "-c".into(),
            program.into(),
        ],
        log_path: format!("{name}.log"),
        linux: Some(LinuxContainerConfig {
            security_context: Some(LinuxContainerSecurityContext {
                namespace_options: Some(namespaces()),
                no_new_privs: true,
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}
async fn create(
    cli: &mut Runtime,
    pod: &str,
    config: &PodSandboxConfig,
    work: ContainerConfig,
) -> Result<String> {
    Ok(cli
        .create_container(CreateContainerRequest {
            pod_sandbox_id: pod.into(),
            config: Some(work),
            sandbox_config: Some(config.clone()),
        })
        .await?
        .into_inner()
        .container_id)
}
async fn state(cli: &mut Runtime, id: &str) -> Result<ContainerStatus> {
    cli.container_status(ContainerStatusRequest {
        container_id: id.into(),
        verbose: true,
    })
    .await?
    .into_inner()
    .status
    .context("missing ContainerStatus")
}
fn exec_request(id: &str, program: &str, seconds: i64) -> ExecSyncRequest {
    ExecSyncRequest {
        container_id: id.into(),
        cmd: vec![
            "/bin/busybox".into(),
            "sh".into(),
            "-c".into(),
            program.into(),
        ],
        timeout: seconds,
    }
}
async fn exec(cli: &mut Runtime, id: &str, program: &str) -> Result<Vec<u8>> {
    let r = cli
        .exec_sync(exec_request(id, program, 5))
        .await?
        .into_inner();
    ensure!(
        r.exit_code == 0,
        "exec {program:?} exited {}: {:?}",
        r.exit_code,
        String::from_utf8_lossy(&r.stderr)
    );
    Ok(r.stdout)
}

async fn scenario(
    root: &Path,
    image: &str,
    daemon_process: &mut Child,
    cli: &mut Runtime,
) -> Result<()> {
    let image_id = ImageServiceClient::new(channel(root))
        .pull_image(PullImageRequest {
            image: Some(ImageSpec {
                image: image.into(),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .context("pull locally cached immutable OCI manifest")?
        .into_inner()
        .image_ref;
    ensure!(image_id.starts_with("sha256:"), "bad image ID {image_id}");

    let pod = pod_config(root, "primary");
    let id = boot(cli, &pod).await.context("CRI RunPodSandbox primary")?;
    let boot_a = boot_id(cli, &id).await?;
    let other = boot(cli, &pod_config(root, "other")).await?;
    let boot_b = boot_id(cli, &other).await?;
    ensure!(boot_a != boot_b);
    let init="echo init-out; echo init-err >&2; while test ! -e finish; do /bin/busybox sleep .1; done; exit 37";
    let mut first_work = workload("first", image, init);
    first_work.metadata.as_mut().unwrap().attempt = 2;
    let mut unsupported = first_work.clone();
    unsupported
        .linux
        .as_mut()
        .unwrap()
        .security_context
        .as_mut()
        .unwrap()
        .privileged = true;
    ensure!(
        create(cli, &id, &pod, unsupported).await.is_err(),
        "accepted privileged request"
    );
    let mut mount = first_work.clone();
    mount.mounts.push(Mount {
        host_path: "/".into(),
        container_path: "/host".into(),
        ..Default::default()
    });
    ensure!(
        create(cli, &id, &pod, mount).await.is_err(),
        "accepted unrestricted host mount"
    );
    // Fail after the host has accepted the config and begun preparing a guest
    // bundle. Kubelet retries this same log_path when the immutable image arrives.
    let mut missing = first_work.clone();
    missing.image.as_mut().unwrap().image =
        format!("127.0.0.1:1/test/absent@sha256:{}", "0".repeat(64));
    ensure!(
        create(cli, &id, &pod, missing).await.is_err(),
        "accepted missing image"
    );
    let a = create(cli, &id, &pod, first_work)
        .await
        .context("create first")?;
    let b = create(
        cli,
        &id,
        &pod,
        workload(
            "second",
            image,
            "echo second-out; exec /bin/busybox sleep 300",
        ),
    )
    .await
    .context("create second")?;
    let initial = state(cli, &a).await?;
    ensure!(initial.image_id == image_id && initial.metadata.as_ref().unwrap().attempt == 2);
    for c in [&a, &b] {
        ensure!(state(cli, c).await?.state() == ContainerState::ContainerCreated);
        cli.start_container(StartContainerRequest {
            container_id: c.clone(),
        })
        .await?;
        ensure!(state(cli, c).await?.state() == ContainerState::ContainerRunning);
        ensure!(
            exec(cli, c, "/bin/busybox cat /proc/sys/kernel/random/boot_id").await?
                == format!("{boot_a}\n").as_bytes()
        );
    }
    ensure!(
        exec(cli, &a, "/bin/busybox readlink /proc/self/ns/pid").await?
            != exec(cli, &b, "/bin/busybox readlink /proc/self/ns/pid").await?
    );
    for n in 1..=10 {
        let out = exec(cli, &a, "echo x >> count; /bin/busybox wc -l < count").await?;
        ensure!(std::str::from_utf8(&out)?.trim() == n.to_string());
    }
    ensure!(exec(cli, &a, "pwd; echo $IMAGE_VAR").await? == b"/workspace\noci-cache\n");
    exec(cli, &b, "test ! -e count").await?;
    let nonzero = cli
        .exec_sync(exec_request(&a, "echo out; echo err >&2; exit 23", 0))
        .await?
        .into_inner();
    ensure!(nonzero.stdout == b"out\n" && nonzero.stderr == b"err\n" && nonzero.exit_code == 23);

    let deadline = cli
        .exec_sync(exec_request(
            &a,
            "echo started > timeout-started; /bin/busybox sleep 4; echo leak > timed-out",
            1,
        ))
        .await;
    ensure!(deadline.is_err(), "ExecSync exceeded timeout successfully");
    let mut canceled_client = cli.clone();
    let cancel_request = exec_request(
        &a,
        "echo started > cancel-started; /bin/busybox sleep 2; echo leak > canceled",
        0,
    );
    let canceled = tokio::spawn(async move { canceled_client.exec_sync(cancel_request).await });
    timeout(Duration::from_secs(4), async {
        loop {
            let r = cli
                .exec_sync(exec_request(&a, "test -f cancel-started", 5))
                .await?
                .into_inner();
            if r.exit_code == 0 {
                return Ok::<_, anyhow::Error>(());
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .context("exec never started before cancellation")??;
    canceled.abort();
    let _ = canceled.await;
    sleep(Duration::from_millis(3200)).await;
    exec(
        cli,
        &a,
        "test -f timeout-started && test ! -e timed-out && test ! -e canceled",
    )
    .await?;

    // Disconnect only the primary Pod's host transport while both VMs run. Restore
    // the original socket inode before propagating either CRI result.
    let socket = root.join("state/vms").join(&id).join("vsock.sock");
    let detached = socket.with_file_name("detached-vsock.sock");
    fs::rename(&socket, &detached)?;
    let disconnected = exec(cli, &a, "echo unexpected").await;
    let observed = state(cli, &a).await;
    let listing = cli.list_containers(ListContainersRequest::default()).await;
    let other_boot = boot_id(cli, &other).await;
    fs::rename(&detached, &socket)?;
    ensure!(disconnected.is_err(), "exec accepted a disconnected guest");
    ensure!(other_boot? == boot_b);
    ensure!(observed?.state() == ContainerState::ContainerUnknown);
    ensure!(listing?
        .into_inner()
        .containers
        .iter()
        .any(|c| c.id == a && c.state() == ContainerState::ContainerUnknown));
    ensure!(state(cli, &b).await?.state() == ContainerState::ContainerRunning);
    exec(cli, &b, "test ! -e count").await?;

    daemon_process.kill().await?;
    let (new_daemon, new_client) = daemon(root).await?;
    *daemon_process = new_daemon;
    *cli = new_client;
    ensure!(boot_id(cli, &id).await? == boot_a && boot_id(cli, &other).await? == boot_b);
    let after = exec(cli, &a, "/bin/busybox wc -l < count").await?;
    ensure!(
        std::str::from_utf8(&after)?.trim() == "10",
        "lost workspace after restart"
    );
    exec(cli, &b, "test ! -e count").await?;
    exec(cli, &a, "echo done > finish").await?;
    let exited = timeout(Duration::from_secs(8), async {
        loop {
            let s = state(cli, &a).await?;
            if s.state() == ContainerState::ContainerExited {
                return Ok::<_, anyhow::Error>(s);
            }
            sleep(Duration::from_millis(70)).await;
        }
    })
    .await
    .context("wait for real init exit")??;
    ensure!(
        exited.exit_code == 37
            && 0 < exited.created_at
            && exited.created_at <= exited.started_at
            && exited.started_at <= exited.finished_at,
        "bad exit status {exited:?}"
    );
    for (file, expected, other) in [
        ("first.log", "stdout F init-out", "second-out"),
        ("second.log", "stdout F second-out", "init-out"),
    ] {
        let log = fs::read_to_string(Path::new(&pod.log_directory).join(file))?;
        let line = log
            .lines()
            .find(|line| line.ends_with(expected))
            .with_context(|| format!("missing {expected} in {file}"))?;
        chrono::DateTime::parse_from_rfc3339(
            line.split_once(' ').context("missing CRI timestamp")?.0,
        )?;
        ensure!(!log.contains(other), "mixed container logs");
        if file == "first.log" {
            ensure!(log.lines().any(|l| l.ends_with("stderr F init-err")))
        }
    }
    for c in [&a, &b] {
        for _ in 0..2 {
            cli.stop_container(StopContainerRequest {
                container_id: c.clone(),
                timeout: 0,
            })
            .await?;
        }
        for _ in 0..2 {
            cli.remove_container(RemoveContainerRequest {
                container_id: c.clone(),
            })
            .await?;
        }
        if c == &a {
            exec(cli, &b, "test ! -e count").await?;
        }
    }
    ensure!(boot_id(cli, &other).await? == boot_b);
    Ok(())
}

async fn cleanup(cli: &mut Runtime) -> Result<()> {
    let pods = cli
        .list_pod_sandbox(ListPodSandboxRequest::default())
        .await?
        .into_inner()
        .items;
    let mut errors = Vec::new();
    for pod in pods {
        for _ in 0..2 {
            if let Err(e) = cli
                .stop_pod_sandbox(StopPodSandboxRequest {
                    pod_sandbox_id: pod.id.clone(),
                })
                .await
            {
                errors.push(e.to_string());
            }
            if let Err(e) = cli
                .remove_pod_sandbox(RemovePodSandboxRequest {
                    pod_sandbox_id: pod.id.clone(),
                })
                .await
            {
                errors.push(e.to_string());
            }
        }
    }
    ensure!(errors.is_empty(), "CRI teardown: {errors:?}");
    ensure!(cli
        .list_containers(ListContainersRequest::default())
        .await?
        .into_inner()
        .containers
        .is_empty());
    ensure!(cli
        .list_pod_sandbox(ListPodSandboxRequest::default())
        .await?
        .into_inner()
        .items
        .is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires systemd, KVM, workload guest and bridge CNI plugins"]
async fn vm_workloads_through_cri_recover_after_daemon_restart() -> Result<()> {
    // One private host network namespace for both daemon processes keeps their
    // bridge/IPAM and NAT changes separate while preserving restart semantics.
    if std::env::var_os("NAUTILUS_TEST_IN_CRI_NETNS").is_none() {
        let status = Command::new("unshare")
            .args(["--net", "--"])
            .arg(std::env::current_exe()?)
            .args([
                "--ignored",
                "--exact",
                "vm_workloads_through_cri_recover_after_daemon_restart",
                "--nocapture",
            ])
            .env("NAUTILUS_TEST_IN_CRI_NETNS", "1")
            .status()
            .await?;
        ensure!(status.success(), "isolated scenario failed: {status}");
        return Ok(());
    }
    let dir = tempfile::tempdir_in("/tmp")?;
    let root = dir.path();
    let image = configure(root)?;
    let (mut process, mut cli) = daemon(root).await?;
    let result = scenario(root, &image, &mut process, &mut cli).await;
    let cleaned = cleanup(&mut cli).await;
    let _ = process.kill().await;
    if let Err(e) = cleaned {
        let saved = dir.into_path();
        bail!(
            "scenario {result:?}; cleanup {e:#}; retained test state at {}",
            saved.display()
        )
    }
    result
}
