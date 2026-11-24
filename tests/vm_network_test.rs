use nautilus::network::{vm::VmNetwork, PodNet, PodNetInfo};
use nautilus::proto::runtime::v1::DnsConfig;
use nautilus::Config;
use serde_json::json;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn config(dir: &Path) -> Config {
    let mut config = Config::default();
    config.runtime.state_dir = dir.join("state");
    config.network.netns_dir = dir.join("netns");
    config.network.cni_config_dir = dir.join("cni");
    config.network.cni_bin_dir = dir.join("bin");
    fs::create_dir(&config.network.cni_config_dir).unwrap();
    fs::create_dir(&config.network.cni_bin_dir).unwrap();
    config
}

#[tokio::test]
async fn vm_requires_explicit_supported_cni_before_reserving_a_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let net = PodNet::new(Arc::new(config.clone()));
    let id = Uuid::new_v4().to_string();
    assert!(net.reserve_vm(&id).is_err());
    fs::write(
        config.network.cni_config_dir.join("test.conf"),
        r#"{"name":"host","type":"host-device"}"#,
    )
    .unwrap();
    assert!(net.reserve_vm(&id).is_err());
    assert!(!config.network.netns_dir.exists());
    net.teardown_vm(&id).await.unwrap();
}

// Run this binary inside a disposable mount and network namespace. A test-only
// PID 1 starts /init (the production agent), then resolves and pings its peer
// using only the Pod resolver and NIC configured by that agent.
fn network_test_image(dir: &Path) -> PathBuf {
    let input = std::env::var("NAUTILUS_VM_INITRAMFS").unwrap();
    let script = dir.join("network-test");
    fs::write(
        &script,
        r#"#!/bin/busybox sh
/bin/busybox mkdir -p /dev
/bin/busybox mount -t devtmpfs devtmpfs /dev
/init &
for i in $(/bin/busybox seq 60); do
    if [ -f /etc/resolv.conf ] &&
       /bin/busybox ip route | /bin/busybox grep -q "^default via 10.231.13.1 dev eth0" &&
       /bin/busybox nslookup -type=a "$netpeer" >/dev/null 2>&1 &&
       /bin/busybox ping -c 1 -W 1 "$netpeer"; then
        echo "NETWORK_TEST_OK $netpeer"
        break
    fi
    /bin/busybox sleep 0.2
done
exec /bin/busybox sleep 300
"#,
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let output = dir.join("network.cpio.gz");
    fs::copy(input, &output).unwrap();
    let status = Command::new("bash").args(["-euo", "pipefail", "-c",
        "printf '%s\\0' network-test | cpio --quiet --null -o -H newc --owner=0:0 | gzip -n >>network.cpio.gz"])
        .current_dir(dir).status().unwrap();
    assert!(status.success());
    output
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn launch(
    dir: &Path,
    image: &Path,
    id: &str,
    network: &VmNetwork,
    peer: &str,
) -> (Process, PathBuf, String) {
    let log = dir.join(format!("{id}.serial"));
    let nonce = Uuid::new_v4();
    let cmdline = format!("rdinit=/network-test panic=1 console=ttyS0 netpeer={peer} nautilus.sandbox_id={id} nautilus.nonce={nonce} {}", network.cmdline);
    let child = Command::new("nsenter")
        .arg(format!("--net={}", network.netns_path))
        .arg("--")
        .arg(std::env::var("NAUTILUS_VM_VMM").unwrap())
        .arg("--kernel")
        .arg(std::env::var("NAUTILUS_VM_KERNEL").unwrap())
        .arg("--initramfs")
        .arg(image)
        .args([
            "--cpus",
            "boot=1",
            "--memory",
            "size=256M",
            "--console",
            "off",
        ])
        .args(["--net", &format!("tap=nttap0,mac={}", network.mac)])
        .args([
            "--vsock",
            &format!("cid=3,socket={}", dir.join(format!("{id}.sock")).display()),
        ])
        .args([
            "--serial",
            &format!("file={}", log.display()),
            "--cmdline",
            &cmdline,
        ])
        .spawn()
        .unwrap();
    (Process(child), log, nonce.to_string())
}

async fn allocate(net: &PodNet, id: &str, dns: Option<&DnsConfig>) -> (PodNetInfo, VmNetwork) {
    let mut info = net.reserve_vm(id).unwrap();
    let network = net.attach_vm(id, &mut info, dns).await.unwrap();
    (info, network)
}

#[tokio::test]
#[ignore = "two real KVM guests, isolated network and mount namespaces, CNI binaries, dnsmasq"]
async fn real_guests_resolve_and_reach_one_another_and_release_only_their_allocations() {
    for kind in ["net", "mnt"] {
        assert_ne!(
            fs::read_link(format!("/proc/1/ns/{kind}")).unwrap(),
            fs::read_link(format!("/proc/self/ns/{kind}")).unwrap(),
            "run with sudo unshare -mn --propagation private"
        );
    }
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let config = config(dir.path());
    let plugins = PathBuf::from(std::env::var("NAUTILUS_CNI_BIN_DIR").unwrap());
    for p in ["loopback", "bridge", "host-local"] {
        symlink(plugins.join(p), config.network.cni_bin_dir.join(p)).unwrap();
    }
    let leases = dir.path().join("leases");
    let conflist = config.network.cni_config_dir.join("test.conflist");
    let chain = json!({"cniVersion":"0.4.0","name":"network13","plugins":[
        {"type":"loopback"},
        {"type":"bridge","bridge":"nttest13","isGateway":true,"ipam":{"type":"host-local",
            "subnet":"10.231.13.0/24","routes":[{"dst":"0.0.0.0/0"}],"dataDir":leases},
            "dns":{"nameservers":["10.231.13.1"]}}
    ]});
    fs::write(&conflist, chain.to_string()).unwrap();
    let net = PodNet::new(Arc::new(config.clone()));
    let a = Uuid::new_v4().to_string();
    let b = Uuid::new_v4().to_string();
    let dns = DnsConfig {
        servers: vec!["10.231.13.1".into()],
        searches: vec!["svc.test".into()],
        options: vec!["ndots:1".into()],
    };
    let replaced = Uuid::new_v4().to_string();
    let handle = net.reserve_vm(&replaced).unwrap();
    nix::mount::umount(handle.netns_path.as_str()).unwrap();
    let backup = dir.path().join("original-placeholder");
    fs::rename(&handle.netns_path, &backup).unwrap();
    fs::write(&handle.netns_path, "foreign").unwrap();
    assert!(net.teardown_vm(&replaced).await.is_err());
    assert_eq!(fs::read_to_string(&handle.netns_path).unwrap(), "foreign");
    fs::remove_file(&handle.netns_path).unwrap();
    symlink("/missing-vm-test-path", &handle.netns_path).unwrap();
    assert!(net.teardown_vm(&replaced).await.is_err());
    assert!(fs::symlink_metadata(&handle.netns_path)
        .unwrap()
        .is_symlink());
    fs::remove_file(&handle.netns_path).unwrap();
    fs::rename(&backup, &handle.netns_path).unwrap();
    net.teardown_vm(&replaced).await.unwrap();

    // A CNI ADD inherited the operation lock before its caller was canceled.
    // DEL must wait until the plugin has finished allocating.
    let delayed = Uuid::new_v4().to_string();
    let mut delayed_info = net.reserve_vm(&delayed).unwrap();
    let bridge = config.network.cni_bin_dir.join("bridge");
    fs::rename(&bridge, bridge.with_file_name("real-bridge")).unwrap();
    fs::write(
        &bridge,
        r#"#!/bin/sh
if [ "$CNI_COMMAND" = ADD ]; then
    touch "$CNI_PATH/adding"
    while [ ! -e "$CNI_PATH/release" ]; do sleep 0.02; done
fi
exec "$CNI_PATH/real-bridge"
"#,
    )
    .unwrap();
    fs::set_permissions(&bridge, fs::Permissions::from_mode(0o755)).unwrap();
    let other = PodNet::new(Arc::new(config.clone()));
    let delayed_id = delayed.clone();
    let add =
        tokio::spawn(async move { other.attach_vm(&delayed_id, &mut delayed_info, None).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !config.network.cni_bin_dir.join("adding").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    add.abort();
    assert!(add.await.unwrap_err().is_cancelled());
    let other = PodNet::new(Arc::new(config.clone()));
    let del = tokio::spawn(async move { other.teardown_vm(&delayed).await });
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        !del.is_finished(),
        "DEL ran before the outstanding ADD finished"
    );
    fs::write(config.network.cni_bin_dir.join("release"), "").unwrap();
    del.await.unwrap().unwrap();
    fs::remove_file(&bridge).unwrap();
    fs::rename(bridge.with_file_name("real-bridge"), &bridge).unwrap();
    assert!(!leases.join("network13").join("10.231.13.2").exists());
    let (first, first_nic) = allocate(&net, &a, Some(&dns)).await;
    let (second, second_nic) = allocate(&net, &b, None).await;
    assert_ne!(first.ip, second.ip);
    assert!(first_nic.resolv_conf.contains("search svc.test"));
    assert_eq!(second_nic.resolv_conf, "nameserver 10.231.13.1\n");
    // A bad resolver fails after CNI ADD. Reopening the journal releases
    // exactly that IPAM reservation even without a completed VM or Pod IP.
    let failed = Uuid::new_v4().to_string();
    let mut partial = net.reserve_vm(&failed).unwrap();
    let bad_dns = DnsConfig {
        servers: vec!["invalid".into()],
        ..Default::default()
    };
    assert!(net
        .attach_vm(&failed, &mut partial, Some(&bad_dns))
        .await
        .is_err());
    let failed_lease = fs::read_dir(leases.join("network13"))
        .unwrap()
        .flatten()
        .map(|f| f.path())
        .find(|p| fs::read_to_string(p).is_ok_and(|s| s.lines().next() == Some(&failed)))
        .unwrap();
    PodNet::new(Arc::new(config.clone()))
        .teardown_vm(&failed)
        .await
        .unwrap();
    assert!(!failed_lease.exists());
    assert!(!Path::new(&partial.netns_path).exists());
    let _dns = Process(
        Command::new("dnsmasq")
            .args([
                "--no-daemon",
                "--no-resolv",
                "--no-hosts",
                "--bind-interfaces",
                "--listen-address=10.231.13.1",
                &format!("--address=/first.svc.test/{}", first.ip),
                &format!("--address=/second.svc.test/{}", second.ip),
            ])
            .spawn()
            .unwrap(),
    );
    let image = network_test_image(dir.path());
    let (guest_a, log_a, nonce_a) = launch(dir.path(), &image, &a, &first_nic, "second.svc.test");
    let (guest_b, log_b, nonce_b) = launch(dir.path(), &image, &b, &second_nic, "first.svc.test");
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let l = fs::read_to_string(&log_a).unwrap_or_default();
        let r = fs::read_to_string(&log_b).unwrap_or_default();
        if l.contains("NETWORK_TEST_OK second.svc.test")
            && r.contains("NETWORK_TEST_OK first.svc.test")
        {
            break;
        }
        assert!(Instant::now() < deadline, "guest A: {l}\nguest B: {r}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for (id, nonce, nic) in [(&a, &nonce_a, &first_nic), (&b, &nonce_b, &second_nic)] {
        let socket = dir.path().join(format!("{id}.sock"));
        let boot = nautilus::vm::guest::identify(&socket, id, nonce)
            .await
            .unwrap();
        nic.verify_guest(&socket, &boot).await.unwrap();
        assert!(nic
            .verify_guest(&socket, &Uuid::new_v4().to_string())
            .await
            .is_err());
    }
    println!(
        "bidirectional DNS and ICMP via guest IPs {} and {}",
        first.ip, second.ip
    );
    drop(guest_a);
    fs::remove_file(&conflist).unwrap();
    let resumed = PodNet::new(Arc::new(config.clone()));
    let plugin = config.network.cni_bin_dir.join("bridge");
    // Overlay a different Pod's namespace on A's path. No DEL may be sent
    // there: both sites' leases must remain until the original is visible.
    nix::mount::mount(
        Some(second.netns_path.as_str()),
        first.netns_path.as_str(),
        None::<&str>,
        nix::mount::MsFlags::MS_BIND,
        None::<&str>,
    )
    .unwrap();
    assert!(resumed.teardown_vm(&a).await.is_err());
    assert!(leases.join("network13").join(&first.ip).exists());
    assert!(leases.join("network13").join(&second.ip).exists());
    nix::mount::umount(first.netns_path.as_str()).unwrap();
    fs::remove_file(&plugin).unwrap();
    assert!(resumed.teardown_vm(&a).await.is_err());
    assert!(Path::new(&first.netns_path).exists());
    symlink(plugins.join("bridge"), &plugin).unwrap();
    resumed.teardown_vm(&a).await.unwrap();
    resumed.teardown_vm(&a).await.unwrap();
    assert!(!Path::new(&first.netns_path).exists());
    assert!(!leases.join("network13").join(&first.ip).exists());
    assert!(leases.join("network13").join(&second.ip).exists());
    assert!(Command::new("ping")
        .args(["-c1", "-W1", &second.ip])
        .status()
        .unwrap()
        .success());
    drop(guest_b);
    resumed.teardown_vm(&b).await.unwrap();
    assert!(!Path::new(&second.netns_path).exists());
    assert!(!leases.join("network13").join(&second.ip).exists());
}
