//! Run on an isolated root/systemd/KVM Linux host with a workload initramfs:
//! NAUTILUS_VM_{VMM,KERNEL,INITRAMFS,VIRTIOFSD}, NAUTILUS_CNI_BIN_DIR and
//! NAUTILUS_TEST_BUSYBOX (static). NAUTILUS_TEST_DAEMON selects a copied daemon.
//! Set NAUTILUS_TEST_CRICTL to an upstream crictl binary (verified with v1.37.0)
//! for live SPDY checks. The fixture uses host python3 and iproute2 in its private network namespace.
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
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
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
    for path in ["workspace/image-file", "workspace/image-delete"] {
        let content = b"from-image\n";
        let mut h = tar::Header::new_gnu();
        h.set_size(content.len() as u64);
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        tar.append_data(&mut h, path, content.as_slice())?;
    }
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
    timeout(Duration::from_secs(75), async {
        loop {
            if let Some(status) = child.try_wait()? {
                bail!("daemon startup failed: {status}")
            }
            // The CRI socket opens while the hidden readiness VM is starting.
            // Advertising the VM handler follows removal of that probe and its network.
            if cli
                .status(StatusRequest::default())
                .await
                .is_ok_and(|status| {
                    status
                        .into_inner()
                        .runtime_handlers
                        .iter()
                        .any(|handler| handler.name == "vm")
                })
            {
                return Ok::<_, anyhow::Error>(());
            }
            sleep(Duration::from_millis(80)).await;
        }
    })
    .await
    .context("waiting for test daemon to advertise the VM handler")??;
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
        labels: [("test-pod".into(), name.into())].into(),
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
        labels: [("test-container".into(), name.into())].into(),
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

// Simulate daemon death directly after its placeholder receipt became durable
// but before renameat2. A public file of exactly the expected type and size is
// still someone else's. Cleanup must release only the private staged inode.
fn interrupted_volume(root: &Path, pod: &str, container: &str) -> Result<(PathBuf, PathBuf, u64)> {
    let store = root.join("state/vms");
    let slot = Uuid::new_v4().to_string();
    let stage = store.join(pod).join(".volume-stage").join(&slot);
    let public = store.join(pod).join("podfs/volumes").join(&slot);
    use std::os::unix::fs::OpenOptionsExt;
    let create_empty = |path: &Path| {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    };
    create_empty(&stage)?.sync_all()?;
    let foreign = create_empty(&public)?;
    let metadata = fs::metadata(&stage)?;
    let identity = json!({"device":metadata.dev(), "inode":metadata.ino()});
    let record = store.join(format!("{pod}.json"));
    let mut owner: serde_json::Value = serde_json::from_slice(&fs::read(&record)?)?;
    owner["share"]["bindings"]
        .as_array_mut()
        .context("volume receipts")?
        .push(json!({
            "id":slot, "container":container, "mount":0, "source":identity, "placeholder":identity
        }));
    fs::write(record, serde_json::to_vec(&owner)?)?;
    Ok((stage, public, foreign.metadata()?.ino()))
}

async fn unpublished_rootfs_removal(root: &Path, pod: &str) -> Result<()> {
    let vms = root.join("state/vms");
    let container = Uuid::new_v4().to_string();
    let staged = vms.join(format!(".oci-{container}"));
    let public = vms.join(pod).join("podfs/containers").join(&container);
    fs::DirBuilder::new().mode(0o700).create(&staged)?;
    fs::create_dir(&public)?;
    fs::write(public.join("contents"), b"existing bundle")?;
    let metadata = fs::metadata(&staged)?;
    let identity = json!({"device":metadata.dev(), "inode":metadata.ino()});
    let record = vms.join(format!("{pod}.json"));
    let mut owner: serde_json::Value = serde_json::from_slice(&fs::read(&record)?)?;
    owner["share"]["rootfs"]
        .as_array_mut()
        .context("rootfs receipts")?
        .push(json!({
            "container":container, "mount":0, "bundle":identity,
            "placeholder":identity, "root":identity
        }));
    fs::write(record, serde_json::to_vec(&owner)?)?;
    let store = nautilus::vm::owner::PodVmStore::new(vms);
    store.remove_bundle(pod, &container).await?;
    fs::remove_dir(&staged)?;
    // Restarted cleanup can retry after the staging inode has been removed.
    store.remove_bundle(pod, &container).await?;
    ensure!(fs::read(public.join("contents"))? == b"existing bundle");
    fs::remove_dir_all(public)?;
    Ok(())
}

// These host mounts are confined to the disposable fixture. Drop them before
// TempDir cleanup, including on failure.
struct FixtureMount(PathBuf);
impl FixtureMount {
    fn new(source: &Path, target: &Path) -> Result<Self> {
        nix::mount::mount(
            Some(source),
            target,
            None::<&str>,
            nix::mount::MsFlags::MS_BIND,
            None::<&str>,
        )?;
        Ok(Self(target.into()))
    }
}
impl Drop for FixtureMount {
    fn drop(&mut self) {
        nix::mount::umount2(&self.0, nix::mount::MntFlags::MNT_DETACH)
            .expect("unmount temporary fixture");
    }
}

async fn container_usage(cli: &mut Runtime, id: &str) -> Result<ContainerStats> {
    Ok(cli
        .container_stats(ContainerStatsRequest {
            container_id: id.into(),
        })
        .await?
        .into_inner()
        .stats
        .context("missing container stats")?)
}

async fn guest_stats(
    cli: &mut Runtime,
    pod: &str,
    other: &str,
    busy: &str,
    idle: &str,
) -> Result<()> {
    let counters = |s: &ContainerStats| -> Result<(u64, u64)> {
        let cpu = s.cpu.as_ref().context("missing CPU")?;
        let mem = s.memory.as_ref().context("missing memory")?;
        ensure!(cpu.timestamp > 0 && cpu.timestamp == mem.timestamp);
        let usage = mem
            .usage_bytes
            .as_ref()
            .context("missing memory.current")?
            .value;
        ensure!(
            mem.working_set_bytes
                .as_ref()
                .context("missing working set")?
                .value
                <= usage
        );
        Ok((
            cpu.usage_core_nano_seconds
                .as_ref()
                .context("missing CPU nanoseconds")?
                .value,
            usage,
        ))
    };
    let before_busy = counters(&container_usage(cli, busy).await?)?.0;
    let before_idle = counters(&container_usage(cli, idle).await?)?.0;
    // The busy container charges persistent tmpfs pages and consumes CPU in exec descendants.
    exec(cli, busy, "/bin/busybox dd if=/dev/zero of=/dev/shm/stats-load bs=1048576 count=16 2>/dev/null || exit 1; i=0; while [ $i -lt 12 ]; do /bin/busybox sha256sum /dev/shm/stats-load >/dev/null || exit 1; i=$((i+1)); done").await?;
    sleep(Duration::from_secs(1)).await;
    let busy_sample = container_usage(cli, busy).await?;
    let (busy_cpu, busy_mem) = counters(&busy_sample)?;
    let (idle_cpu, idle_mem) = counters(&container_usage(cli, idle).await?)?;
    ensure!(
        busy_cpu - before_busy > (idle_cpu - before_idle) + 1_000_000,
        "busy/idle CPU: {busy_cpu}/{idle_cpu}"
    );
    ensure!(
        busy_mem > idle_mem + (8 << 20),
        "busy/idle memory: {busy_mem}/{idle_mem}"
    );
    ensure!(
        busy_sample
            .cpu
            .unwrap()
            .usage_nano_cores
            .context("missing second-sample CPU rate")?
            .value
            > 0
    );
    let stats = cli
        .pod_sandbox_stats(PodSandboxStatsRequest {
            pod_sandbox_id: pod.into(),
        })
        .await?
        .into_inner()
        .stats
        .context("missing Pod stats")?;
    let name = stats
        .attributes
        .as_ref()
        .context("missing Pod attributes")?
        .metadata
        .as_ref()
        .unwrap()
        .name
        .clone();
    let linux = stats.linux.context("missing Linux Pod stats")?;
    ensure!(linux.cpu.unwrap().usage_core_nano_seconds.unwrap().value >= busy_cpu + idle_cpu);
    ensure!(linux.memory.unwrap().usage_bytes.unwrap().value >= busy_mem);
    let mut children: Vec<_> = linux
        .containers
        .iter()
        .map(|c| c.attributes.as_ref().unwrap().id.as_str())
        .collect();
    children.sort_unstable();
    let mut expected = [busy, idle];
    expected.sort_unstable();
    ensure!(children == expected);
    let all = cli
        .list_container_stats(ListContainerStatsRequest::default())
        .await?
        .into_inner()
        .stats;
    for id in [busy, idle] {
        ensure!(all
            .iter()
            .any(|s| s.attributes.as_ref().is_some_and(|a| a.id == id)));
    }
    let all_pods = cli
        .list_pod_sandbox_stats(ListPodSandboxStatsRequest::default())
        .await?
        .into_inner()
        .stats;
    ensure!(all_pods
        .iter()
        .any(|s| s.attributes.as_ref().is_some_and(|a| a.id == pod)));
    let empty = all_pods
        .iter()
        .find(|s| s.attributes.as_ref().is_some_and(|a| a.id == other))
        .context("missing empty VM Pod stats")?
        .linux
        .as_ref()
        .context("missing Linux stats")?;
    ensure!(empty.containers.is_empty() && empty.cpu.is_some() && empty.memory.is_some());
    let filter = ContainerStatsFilter {
        pod_sandbox_id: pod.into(),
        id: busy.into(),
        label_selector: [("test-container".into(), "first".into())].into(),
    };
    let matching = cli
        .list_container_stats(ListContainerStatsRequest {
            filter: Some(filter.clone()),
        })
        .await?
        .into_inner()
        .stats;
    ensure!(matching.len() == 1 && matching[0].attributes.as_ref().unwrap().id == busy);
    let mismatch = ContainerStatsFilter {
        pod_sandbox_id: other.into(),
        ..filter
    };
    ensure!(cli
        .list_container_stats(ListContainerStatsRequest {
            filter: Some(mismatch)
        })
        .await?
        .into_inner()
        .stats
        .is_empty());
    let filter = PodSandboxStatsFilter {
        id: pod.into(),
        label_selector: [("test-pod".into(), name)].into(),
    };
    let matching = cli
        .list_pod_sandbox_stats(ListPodSandboxStatsRequest {
            filter: Some(filter.clone()),
        })
        .await?
        .into_inner()
        .stats;
    ensure!(matching.len() == 1 && matching[0].attributes.as_ref().unwrap().id == pod);
    ensure!(cli
        .list_pod_sandbox_stats(ListPodSandboxStatsRequest {
            filter: Some(PodSandboxStatsFilter {
                id: other.into(),
                ..filter
            })
        })
        .await?
        .into_inner()
        .stats
        .is_empty());
    eprintln!("guest counters: busy CPU {busy_cpu}ns memory {busy_mem}B; idle CPU {idle_cpu}ns memory {idle_mem}B");
    Ok(())
}

async fn guest_cgroup_metrics(cli: &mut Runtime, pod: &str, busy: &str, idle: &str) -> Result<()> {
    const THROTTLED: &str = "container_cpu_cfs_throttled_periods_total";
    let throttled = |metrics: &PodSandboxMetrics, id: &str| -> Option<u64> {
        metrics
            .container_metrics
            .iter()
            .find(|c| c.container_id == id)?
            .metrics
            .iter()
            .find(|m| m.name == THROTTLED)?
            .value
            .as_ref()
            .map(|v| v.value)
    };
    let all = cli
        .list_pod_sandbox_metrics(ListPodSandboxMetricsRequest {})
        .await?
        .into_inner()
        .pod_metrics;
    let baseline = all
        .iter()
        .find(|entry| entry.pod_sandbox_id == pod)
        .context("missing VM metrics")?;
    let before = throttled(baseline, busy).context("missing guest CPU throttling counter")?;
    let idle_before = throttled(baseline, idle);

    exec(cli, busy, "/bin/busybox sh -c 'while :; do :; done' & worker=$!; /bin/busybox sleep 2; kill \"$worker\"; wait \"$worker\" || :").await?;
    let all = cli
        .list_pod_sandbox_metrics(ListPodSandboxMetricsRequest {})
        .await?
        .into_inner()
        .pod_metrics;
    let sampled = all
        .iter()
        .find(|entry| entry.pod_sandbox_id == pod)
        .context("missing loaded VM metrics")?;
    let after =
        throttled(sampled, busy).context("missing guest CPU throttling counter under load")?;
    ensure!(
        after > before,
        "limited guest container did not throttle: {before} -> {after}"
    );
    ensure!(
        throttled(sampled, idle) == idle_before,
        "idle guest container throttled"
    );
    ensure!(sampled
        .metrics
        .iter()
        .any(|m| m.name == "container_memory_cache"
            && m.value.as_ref().is_some_and(|v| v.value >= 16 << 20)));

    let mut stream = cli
        .stream_pod_sandbox_metrics(StreamPodSandboxMetricsRequest {})
        .await?
        .into_inner();
    let mut seen = false;
    while let Some(batch) = stream.message().await? {
        for entry in batch.pod_sandbox_metrics {
            if entry.pod_sandbox_id == pod {
                ensure!(!seen && throttled(&entry, busy).is_some_and(|value| value >= after));
                seen = true;
            }
        }
    }
    ensure!(seen, "metrics stream omitted the VM Pod");
    eprintln!("guest CFS throttled periods: {before} -> {after}");
    Ok(())
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
    first_work.linux.as_mut().unwrap().resources = Some(LinuxContainerResources {
        cpu_period: 100_000,
        cpu_quota: 50_000,
        ..Default::default()
    });
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
    // All host paths in this test belong to a disposable fixture outside state/storage.
    let sources = tempfile::tempdir()?;
    let hosts = sources.path().join("kubelet-hosts");
    fs::write(&hosts, "127.0.0.1 fixture")?;
    std::os::unix::fs::chown(&hosts, Some(65534), Some(65534))?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&hosts, fs::Permissions::from_mode(0o666))?;
    fs::hard_link(&hosts, sources.path().join("hosts-reference"))?;
    let data = sources.path().join("data");
    fs::create_dir(&data)?;
    let nested = sources.path().join("nested-source");
    fs::create_dir(&nested)?;
    fs::write(nested.join("view"), b"nested")?;
    fs::create_dir(data.join("nested"))?;
    fs::write(data.join("nested/view"), b"covered")?;
    fs::create_dir(data.join("later"))?;
    let _existing_mount = FixtureMount::new(&nested, &data.join("nested"))?;
    let projection = sources.path().join("projected");
    for (version, value) in [("..one", "old"), ("..two", "new")] {
        fs::create_dir_all(projection.join(version))?;
        fs::write(projection.join(version).join("key"), value)?;
    }
    std::os::unix::fs::symlink("..one", projection.join("..data"))?;
    std::os::unix::fs::symlink("..data/key", projection.join("key"))?;
    let link = sources.path().join("selector");
    std::os::unix::fs::symlink(&data, &link)?;
    for (source, dest, readonly) in [
        (&hosts, "/etc/hosts", true),
        (&link, "/data", false),
        (&projection, "/projected", true),
    ] {
        first_work.mounts.push(Mount {
            host_path: source.to_string_lossy().into_owned(),
            container_path: dest.into(),
            readonly,
            ..Default::default()
        });
    }
    let mut incomplete = first_work.clone();
    incomplete.metadata.as_mut().unwrap().name = "partial-source".into();
    incomplete.mounts[1].host_path = sources
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    ensure!(create(cli, &id, &pod, incomplete).await.is_err());
    let export = root.join("state/vms").join(&id).join("podfs/volumes");
    ensure!(
        export.read_dir()?.count() == 0,
        "partially attached mounts survived failed create"
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
    let mut unsupported = first_work.clone();
    unsupported.mounts[0].recursive_read_only = true;
    ensure!(create(cli, &id, &pod, unsupported).await.is_err());
    let mut unsupported = first_work.clone();
    unsupported.mounts[0].propagation = MountPropagation::PropagationHostToContainer.into();
    ensure!(create(cli, &id, &pod, unsupported).await.is_err());
    let a = create(cli, &id, &pod, first_work)
        .await
        .context("create first")?;
    let mut second = workload(
        "second",
        image,
        "echo second-out; exec /bin/busybox sleep 300",
    );
    second.mounts.push(Mount {
        host_path: hosts.to_string_lossy().into_owned(),
        container_path: "/shared-hosts".into(),
        readonly: false,
        ..Default::default()
    });
    let b = create(cli, &id, &pod, second)
        .await
        .context("create second")?;
    let bundles = root.join("containers");
    let a_pin = bundles.join(&a).join("snapshot.v1");
    let b_pin = bundles.join(&b).join("snapshot.v1");
    let expect_snapshot = a_pin.exists()
        || b_pin.exists()
        || std::env::var_os("NAUTILUS_VM_TEST_REQUIRE_SNAPSHOT").is_some();
    if expect_snapshot {
        let a_pin = fs::metadata(a_pin).context("first VM root must pin the snapshot")?;
        let b_pin = fs::metadata(b_pin).context("second VM root must pin the snapshot")?;
        ensure!((a_pin.dev(), a_pin.ino()) == (b_pin.dev(), b_pin.ino()));
        let a_upper = fs::metadata(bundles.join(&a).join("upper"))?;
        let b_upper = fs::metadata(bundles.join(&b).join("upper"))?;
        ensure!((a_upper.dev(), a_upper.ino()) != (b_upper.dev(), b_upper.ino()));
    }
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
    exec(
        cli,
        &a,
        "echo copied-up > image-file; /bin/busybox rm image-delete",
    )
    .await?;
    ensure!(exec(cli, &a, "/bin/busybox cat image-file").await? == b"copied-up\n");
    exec(cli, &a, "test ! -e image-delete").await?;
    ensure!(
        exec(cli, &b, "/bin/busybox cat image-file image-delete").await?
            == b"from-image\nfrom-image\n"
    );
    let mut readonly = workload("readonly", image, "exec /bin/busybox sleep 300");
    readonly
        .linux
        .as_mut()
        .unwrap()
        .security_context
        .as_mut()
        .unwrap()
        .readonly_rootfs = true;
    let readonly = create(cli, &id, &pod, readonly).await?;
    cli.start_container(StartContainerRequest {
        container_id: readonly.clone(),
    })
    .await?;
    ensure!(
        exec(
            cli,
            &readonly,
            "if (echo forbidden > image-file) 2>/dev/null; then exit 1; fi; /bin/busybox cat image-file",
        )
        .await? == b"from-image\n"
    );
    cli.remove_container(RemoveContainerRequest {
        container_id: readonly,
    })
    .await?;
    guest_stats(cli, &id, &other, &a, &b).await?;
    guest_cgroup_metrics(cli, &id, &a, &b).await?;
    ensure!(
        exec(cli, &a, "/bin/busybox readlink /proc/self/ns/pid").await?
            != exec(cli, &b, "/bin/busybox readlink /proc/self/ns/pid").await?
    );
    if std::env::var_os("NAUTILUS_TEST_CRICTL").is_some() {
        let mut config = workload("console", image, "echo guest-spdy-only > index.html; /bin/busybox httpd -f -p 127.0.0.1:18080 -h /workspace & n=0; while read v; do n=$((n+1)); echo original-$$-$n-$v; done");
        config.stdin = true;
        config.tty = true;
        let console = create(cli, &id, &pod, config).await?;
        cli.start_container(StartContainerRequest {
            container_id: console.clone(),
        })
        .await?;
        let token_url = cli
            .exec(ExecRequest {
                container_id: a.clone(),
                cmd: vec!["/bin/busybox".into(), "true".into()],
                stdout: true,
                ..Default::default()
            })
            .await?
            .into_inner()
            .url;
        let result = Command::new("python3")
            .arg("-c")
            .arg(include_str!("fixtures/vm_streams/cri.py"))
            .arg(asset("NAUTILUS_TEST_CRICTL")?)
            .arg(root.join("cri.sock"))
            .arg(&id)
            .arg(&a)
            .arg(&console)
            .arg(token_url)
            .kill_on_drop(true)
            .status()
            .await?;
        ensure!(result.success(), "crictl streaming failed: {result}");
        cli.remove_container(RemoveContainerRequest {
            container_id: console,
        })
        .await?;
    }
    ensure!(exec(cli, &a, "/bin/busybox cat /etc/hosts").await? == b"127.0.0.1 fixture");
    ensure!(exec(cli, &a, "/bin/busybox cat /projected/key").await? == b"old");
    exec(
        cli,
        &a,
        "! (echo change > /etc/hosts) && ! (echo change > /projected/key)",
    )
    .await?;
    // The same inode can be requested read-only and read-write concurrently.
    exec(cli, &b, "echo writable > /shared-hosts").await?;
    ensure!(fs::read(&hosts)? == b"writable\n");
    exec(cli, &a, "! (echo change > /etc/hosts)").await?;
    fs::remove_file(&link)?;
    std::os::unix::fs::symlink(&projection, &link)?;
    ensure!(exec(cli, &a, "/bin/busybox cat /data/nested/view").await? == b"nested");
    let _later_mount = FixtureMount::new(&nested, &data.join("later"))?;
    exec(cli, &a, "test ! -e /data/later/view").await?;
    exec(cli, &a, "echo from-guest > /data/token").await?;
    ensure!(fs::read(data.join("token"))? == b"from-guest\n" && !projection.join("token").exists());
    fs::write(data.join("host-token"), b"host")?;
    fs::write(&hosts, b"updated hosts")?;
    std::os::unix::fs::symlink("..two", projection.join("..next"))?;
    fs::rename(projection.join("..next"), projection.join("..data"))?;
    timeout(Duration::from_secs(6), async {
        loop {
            if exec(
                cli,
                &a,
                "/bin/busybox cat /etc/hosts /projected/key /data/host-token",
            )
            .await?
                == b"updated hostsnewhost"
            {
                return Ok::<_, anyhow::Error>(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("guest did not observe updated host files and AtomicWriter symlink swap")??;
    exec(cli, &b, "test ! -e /data/token && test ! -e /projected/key").await?;
    ensure!(export.read_dir()?.count() == 4);
    let mut readonly_files = 0;
    let mut writable_files = 0;
    let fsd = |pod: &str| -> Result<(String, PathBuf)> {
        let text = std::process::Command::new("systemctl")
            .args([
                "show",
                "--value",
                "-p",
                "MainPID",
                &format!("nautilus-virtiofs-{pod}-*.service"),
            ])
            .output()?;
        let pid = String::from_utf8(text.stdout)?.trim().parse::<u32>()?;
        ensure!(pid > 1);
        Ok((
            format!("nautilus-virtiofs-{pod}-*.service"),
            PathBuf::from(format!("/proc/{pid}/root/volumes")),
        ))
    };
    let (_, worker_volumes) = fsd(&id)?;
    let mountinfo = worker_volumes
        .ancestors()
        .nth(2)
        .context("virtiofsd proc directory")?
        .join("mountinfo");
    let mounted_root = |container: &str| -> Result<bool> {
        let suffix = format!("/containers/{container}/rootfs");
        Ok(fs::read_to_string(&mountinfo)?.lines().any(|line| {
            line.split_ascii_whitespace()
                .nth(4)
                .is_some_and(|path| path.ends_with(&suffix))
        }))
    };
    if expect_snapshot {
        ensure!(mounted_root(&a)? && mounted_root(&b)?);
    }
    let (_, mounted) = fsd(&id)?;
    let (_, sibling) = fsd(&other)?;
    for slot in export.read_dir()? {
        let slot = slot?;
        let mount = mounted.join(slot.file_name());
        ensure!(!sibling.join(slot.file_name()).exists());
        if slot.file_type()?.is_file() {
            ensure!(fs::metadata(slot.path())?.len() == 0 && fs::read(&mount)? == b"updated hosts");
            match fs::OpenOptions::new().write(true).open(mount) {
                Err(e) if e.raw_os_error() == Some(libc::EROFS) => readonly_files += 1,
                Ok(_) => writable_files += 1,
                Err(e) => return Err(e.into()),
            }
        } else {
            ensure!(slot.path().read_dir()?.count() == 0);
        }
    }
    ensure!(
        root.join("state/vms")
            .join(&other)
            .join("podfs/volumes")
            .read_dir()?
            .count()
            == 0
    );
    ensure!((readonly_files, writable_files) == (1, 1));
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
    let unavailable = container_usage(cli, &a).await;
    let foreign_stats = cli
        .pod_sandbox_stats(PodSandboxStatsRequest {
            pod_sandbox_id: other.clone(),
        })
        .await;
    let observed = state(cli, &a).await;
    let listing = cli.list_containers(ListContainersRequest::default()).await;
    let other_boot = boot_id(cli, &other).await;
    fs::rename(&detached, &socket)?;
    ensure!(disconnected.is_err(), "exec accepted a disconnected guest");
    ensure!(unavailable.is_err() && foreign_stats?.into_inner().stats.is_some());
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
    let (staging, foreign, foreign_ino) = interrupted_volume(root, &id, &a)?;
    if expect_snapshot {
        unpublished_rootfs_removal(root, &id).await?;
    }
    let (new_daemon, new_client) = daemon(root).await?;
    *daemon_process = new_daemon;
    *cli = new_client;
    ensure!(boot_id(cli, &id).await? == boot_a && boot_id(cli, &other).await? == boot_b);
    ensure!(exec(cli, &a, "/bin/busybox cat image-file").await? == b"copied-up\n");
    exec(cli, &a, "test ! -e image-delete").await?;
    ensure!(
        exec(cli, &b, "/bin/busybox cat image-file image-delete").await?
            == b"from-image\nfrom-image\n"
    );
    ensure!(
        exec(cli, &a, "/bin/busybox cat /projected/key /data/token").await? == b"newfrom-guest\n"
    );
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
        if c == &a {
            let config: serde_json::Value = serde_json::from_slice(&fs::read(
                root.join("state/vms")
                    .join(&id)
                    .join("podfs/containers")
                    .join(&a)
                    .join("config.json"),
            )?)?;
            let selector = config["mounts"][0]["source"]
                .as_str()
                .context("volume selector")?;
            let slot = export.join(selector.strip_prefix("volumes/").context("volume prefix")?);
            let saved = sources.path().join("old-placeholder");
            fs::rename(&slot, &saved)?;
            fs::write(&slot, b"foreign file")?;
            let result = cli
                .remove_container(RemoveContainerRequest {
                    container_id: c.clone(),
                })
                .await;
            ensure!(result.is_err() && fs::read(&slot)? == b"foreign file");
            fs::remove_file(&slot)?;
            fs::rename(saved, &slot)?;
        }
        for _ in 0..2 {
            cli.remove_container(RemoveContainerRequest {
                container_id: c.clone(),
            })
            .await?;
        }
        if expect_snapshot {
            ensure!(!mounted_root(c)? && !bundles.join(c).exists());
        }
        if c == &a {
            ensure!(!staging.exists() && fs::metadata(&foreign)?.ino() == foreign_ino);
            fs::remove_file(&foreign)?;
            ensure!(export.read_dir()?.count() == 1 && fs::read(&hosts)? == b"updated hosts");
            ensure!(exec(cli, &b, "/bin/busybox cat /shared-hosts").await? == b"updated hosts");
            exec(cli, &b, "test ! -e count").await?;
        }
    }
    let never_started = create(cli, &id, &pod, workload("created", image, "exit 93")).await?;
    let created = state(cli, &never_started).await?;
    ensure!(created.state() == ContainerState::ContainerCreated && created.created_at > 0);
    cli.stop_pod_sandbox(StopPodSandboxRequest {
        pod_sandbox_id: id.clone(),
    })
    .await?;
    let stopped = state(cli, &never_started).await?;
    ensure!(
        stopped.state() == ContainerState::ContainerExited
            && stopped.created_at == created.created_at
            && stopped.started_at == 0
            && stopped.finished_at >= stopped.created_at
            && stopped.exit_code == 137,
        "unexpected stopped state for an unstarted guest init: {stopped:?}"
    );
    cli.remove_container(RemoveContainerRequest {
        container_id: never_started,
    })
    .await?;
    // A live binding exists only in its virtiofsd namespace; SIGKILL releases it
    // even while nautilusd is offline. Restart and retry the normal CRI teardown.
    let doomed_config = pod_config(root, "doomed");
    let doomed = boot(cli, &doomed_config).await?;
    let doomed_export = root.join("state/vms").join(&doomed).join("podfs/volumes");
    let (unit, dying_mounts) = fsd(&doomed)?;
    let mut victim = workload("lost-share", image, "exec /bin/busybox sleep 300");
    victim.mounts.push(Mount {
        host_path: hosts.to_string_lossy().into_owned(),
        container_path: "/etc/hosts".into(),
        readonly: true,
        ..Default::default()
    });
    let lost = create(cli, &doomed, &doomed_config, victim).await?;
    cli.start_container(StartContainerRequest { container_id: lost })
        .await?;
    daemon_process.kill().await?;
    ensure!(Command::new("systemctl")
        .args(["kill", "--signal=KILL", &unit])
        .status()
        .await?
        .success());
    timeout(Duration::from_secs(5), async {
        while dying_mounts.exists() {
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .context("virtiofsd namespace survived service exit")?;
    let (restarted, client) = daemon(root).await?;
    *daemon_process = restarted;
    *cli = client;
    for _ in 0..2 {
        cli.stop_pod_sandbox(StopPodSandboxRequest {
            pod_sandbox_id: doomed.clone(),
        })
        .await?;
        cli.remove_pod_sandbox(RemovePodSandboxRequest {
            pod_sandbox_id: doomed.clone(),
        })
        .await?;
    }
    ensure!(!doomed_export.exists() && fs::read(&hosts)? == b"updated hosts");
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

async fn verify_host_cleanup(root: &Path) -> Result<()> {
    let units = Command::new("systemctl")
        .args([
            "show",
            "--property=WorkingDirectory",
            "--value",
            "nautilus-vm-*.service",
            "nautilus-virtiofs-*.service",
        ])
        .output()
        .await?;
    ensure!(
        units.status.success(),
        "systemctl show: {}",
        String::from_utf8_lossy(&units.stderr)
    );
    for dir in std::str::from_utf8(&units.stdout)?.lines() {
        ensure!(
            !Path::new(dir).starts_with(root),
            "test VM service still uses {dir}"
        );
    }

    // The two owners keep a lock file. host-local keeps its lock and allocation cursor.
    for (dir, allowed) in [
        ("state/vms", &[".operations"][..]),
        ("state/vm-network", &[".operations"][..]),
        ("containers", &[][..]),
        ("netns", &[][..]),
        ("ipam/ntcriw15", &["lock", "last_reserved_ip.0"][..]),
    ] {
        let entries = match fs::read_dir(root.join(dir)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            result => result?,
        };
        for entry in entries {
            let entry = entry?;
            if dir == "containers" && entry.file_name() == ".snapshots-v1" {
                continue;
            }
            ensure!(
                entry.file_type()?.is_file()
                    && allowed.iter().any(|&name| entry.file_name() == name),
                "test resource survived cleanup: {}",
                entry.path().display()
            );
        }
    }
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
    ensure!(
        Command::new("ip")
            .args(["link", "set", "lo", "up"])
            .status()
            .await?
            .success(),
        "enable the isolated host namespace loopback for CRI streaming"
    );
    let dir = tempfile::tempdir_in("/tmp")?;
    let image = configure(dir.path())?;
    // Keep the full journal if startup or teardown fails, even with a mounted netns.
    let root = dir.into_path();
    let (mut process, mut cli) = daemon(&root)
        .await
        .with_context(|| format!("starting daemon with retained state at {}", root.display()))?;
    let result = scenario(&root, &image, &mut process, &mut cli).await;
    let cleaned = cleanup(&mut cli).await;
    let stopped = process.kill().await.context("stop test daemon");
    let released = verify_host_cleanup(&root).await;
    if let Err(e) = cleaned.and(stopped).and(released) {
        bail!(
            "scenario {result:?}; cleanup {e:#}; retained test state at {}",
            root.display()
        )
    }
    fs::remove_dir_all(&root)?;
    result
}
