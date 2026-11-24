//! Run on an isolated Linux KVM node with systemd and the assets used in vm_pod_lifecycle_test.
#![cfg(target_os = "linux")]

use anyhow::{ensure, Context, Result};
use nautilus::core::config::VmConfig;
use nautilus::proto::runtime::v1::{LinuxContainerResources, LinuxPodSandboxConfig};
use nautilus::vm::{owner::PodVmStore, resources::VmResources};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires systemd, cgroup v2, real KVM and guest assets"]
async fn pod_limits_reach_vmm_and_kernel() -> Result<()> {
    let config = VmConfig {
        virtiofsd: None,
        vmm: std::env::var("NAUTILUS_VM_VMM")?.into(),
        kernel: std::env::var("NAUTILUS_VM_KERNEL")?.into(),
        initramfs: std::env::var("NAUTILUS_VM_RESOURCE_TEST_INITRAMFS")?.into(),
    };
    let linux = LinuxPodSandboxConfig {
        cgroup_parent: "/system.slice".into(),
        resources: Some(LinuxContainerResources {
            cpu_shares: 1024,
            cpu_period: 100_000,
            cpu_quota: 150_000,
            memory_limit_in_bytes: 448 << 20,
            ..Default::default()
        }),
        overhead: Some(LinuxContainerResources {
            cpu_period: 100_000,
            cpu_quota: 10_000,
            memory_limit_in_bytes: 64 << 20,
            ..Default::default()
        }),
        ..Default::default()
    };
    let dir = tempfile::Builder::new()
        .prefix("nt-limits")
        .tempdir_in("/tmp")?;
    let root = dir.path().join("vms");
    let store = PodVmStore::new(root.clone());
    let id = Uuid::new_v4().to_string();
    let plan = VmResources::from_cri(Some(&linux))?;
    let result = async {
        store.start_with_resources(&id, &config, &plan).await?;
        let mut sock = UnixStream::connect(root.join(&id).join("api.sock")).await?;
        let pid = sock.peer_cred()?.pid().context("missing VMM PID")?;
        let membership = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
        let group = membership
            .lines()
            .find_map(|l| l.strip_prefix("0::/"))
            .context("cgroup v2 required")?;
        ensure!(group.starts_with("system.slice/"));
        let group = Path::new("/sys/fs/cgroup").join(group);
        for (name, expected) in [
            ("cpu.weight", "100"),
            ("cpu.max", "160000 100000"),
            ("memory.max", "536870912"),
        ] {
            let value = std::fs::read_to_string(group.join(name))?;
            ensure!(
                value.trim() == expected,
                "{name}: {value:?} expected {expected}"
            );
        }
        sock.write_all(
            b"GET /api/v1/vm.info HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await?;
        // The API can keep HTTP/1.1 sockets open. Read the declared body length.
        let info = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut buf = Vec::new();
            loop {
                let n = sock.read_buf(&mut buf).await?;
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&buf[..i])?;
                    ensure!(headers.starts_with("HTTP/1.1 200"));
                    let length: usize = headers
                        .lines()
                        .filter_map(|l| l.split_once(':'))
                        .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
                        .context("missing length")?
                        .1
                        .trim()
                        .parse()?;
                    if let Some(body) = buf.get(i + 4..i + 4 + length) {
                        return Ok::<serde_json::Value, anyhow::Error>(serde_json::from_slice(
                            body,
                        )?);
                    }
                }
                ensure!(n > 0 && buf.len() < 1024 * 1024, "incomplete VMM response");
            }
        })
        .await??;
        ensure!(info["config"]["cpus"]["boot_vcpus"] == 2);
        ensure!(info["config"]["memory"]["size"] == (448u64 << 20));
        Ok::<(), anyhow::Error>(())
    }
    .await;
    store.remove(&id).await?;
    result
}
