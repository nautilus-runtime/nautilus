//! A VM gets the CNI endpoint's L2 identity. In its private network namespace,
//! tc redirects frames between that endpoint and the VMM's TAP. The guest is
//! the only IP stack answering on the Pod network.
use super::{PodNet, PodNetInfo};
use crate::core::{Error, Result};
use crate::proto::runtime::v1::DnsConfig;
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Output;
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmNetwork {
    pub netns_path: String,
    pub mac: String,
    pub cmdline: String,
    pub resolv_conf: String,
}

impl VmNetwork {
    /// Legacy identity-only guests cannot acknowledge this request. Compare
    /// both the observed boot and the exact network settings installed by PID 1.
    pub async fn verify_guest(&self, vsock: &Path, boot_id: &str) -> std::io::Result<()> {
        use std::io::{Error as IoError, ErrorKind};
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let expected = format!("NETWORK 1 {boot_id} {}\n", self.cmdline);
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut stream = crate::vm::connect_guest(vsock, 4082).await?;
            stream.get_mut().write_all(b"NETWORK? 1\n").await?;
            let mut reply = vec![0; expected.len()];
            stream.read_exact(&mut reply).await?;
            if reply != expected.as_bytes() {
                return Err(IoError::new(
                    ErrorKind::InvalidData,
                    "guest did not confirm Pod network",
                ));
            }
            Ok(())
        })
        .await
        .map_err(|_| {
            IoError::new(
                ErrorKind::TimedOut,
                "guest Pod network acknowledgement timed out",
            )
        })?
    }
}

type Chain = (String, String, Vec<Value>);

#[derive(Serialize, Deserialize)]
struct Reservation {
    bin_dir: PathBuf,
    chain: Chain,
    netns: PathBuf,
    namespace_id: (u64, u64),
    #[serde(default)]
    placeholder_id: Option<(u64, u64)>,
    adding: bool,
    result: Option<Value>,
}

fn invalid(why: impl Into<String>) -> Error {
    Error::Network(why.into())
}

fn saved(path: &Path, record: &Reservation, create: bool) -> Result<()> {
    let dir = path.parent().expect("VM network record directory");
    let mut next = NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut next, record)?;
    next.as_file().sync_all()?;
    if create {
        next.persist_noclobber(path)
    } else {
        next.persist(path)
    }
    .map_err(|e| e.error)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn read(path: &Path) -> Result<Reservation> {
    serde_json::from_reader(File::open(path)?)
        .map_err(|e| invalid(format!("read VM CNI reservation: {e}")))
}

impl Reservation {
    // None is an absent name. A symlink or a foreign replacement must retain
    // the journal even if it resolves to the original namespace inode.
    fn mounted(&self) -> Result<Option<bool>> {
        let meta = match fs::symlink_metadata(&self.netns) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let id = (meta.dev(), meta.ino());
        if !meta.file_type().is_symlink() {
            if id == self.namespace_id {
                return Ok(Some(true));
            }
            if Some(id) == self.placeholder_id {
                return Ok(Some(false));
            }
        }
        Err(invalid(
            "VM namespace or its placeholder has changed identity",
        ))
    }
}

// The durable operations file is never unlinked. Linux OFD locks follow this
// open file description through dup/fork/exec and release on the last close.
fn try_operation_lock(record: &Path, id: &str) -> std::io::Result<File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(record.with_file_name(".operations"))?;
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = (uuid::Uuid::parse_str(id)
        .expect("validated sandbox id")
        .as_u128() as u64
        % i64::MAX as u64) as i64;
    lock.l_len = 1;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &lock) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

async fn operation_lock(record: &Path, id: &str) -> Result<File> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match try_operation_lock(record, id) {
            Ok(file) => return Ok(file),
            Err(e)
                if matches!(e.raw_os_error(), Some(libc::EAGAIN | libc::EACCES))
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await
            }
            Err(e) => return Err(invalid(format!("acquire VM network operation lease: {e}"))),
        }
    }
}

pub(super) fn keep_operation_lease(cmd: &mut tokio::process::Command, lease: &File) {
    let fd = lease.as_raw_fd();
    unsafe {
        cmd.pre_exec(move || {
            // Change only the child's descriptor flags. Other concurrent
            // spawns in the daemon must keep their descriptors close-on-exec.
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn checked(action: &str, output: Output) -> Result<Vec<u8>> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(invalid(format!(
            "{action}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

async fn in_namespace(lease: &File, ns: &str, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = tokio::process::Command::new("nsenter");
    keep_operation_lease(&mut cmd, lease);
    let out = cmd
        .arg(format!("--net={ns}"))
        .arg("--")
        .args(args)
        .output()
        .await?;
    checked(&args.join(" "), out)
}

impl PodNet {
    fn vm_record(&self, id: &str) -> Result<PathBuf> {
        if !uuid::Uuid::parse_str(id).is_ok_and(|v| !v.is_nil() && v.to_string() == id) {
            return Err(invalid("invalid VM sandbox UUID"));
        }
        Ok(self
            .config
            .runtime
            .state_dir
            .join("vm-network")
            .join(format!("{id}.json")))
    }

    /// Journal the network identity and the exact CNI configuration before
    /// allocating anything. The bind mount alone keeps this namespace alive.
    pub fn reserve_vm(&self, id: &str) -> Result<PodNetInfo> {
        let path = self.vm_record(id)?;
        let chain = self.load_chain(true)?;
        let bin_dir = self.config.network.cni_bin_dir.canonicalize()?;
        let kinds: Vec<_> = chain.2.iter().map(|p| p["type"].as_str()).collect();
        if kinds != [Some("bridge")] && kinds != [Some("loopback"), Some("bridge")] {
            return Err(invalid(
                "VM networking supports a bridge CNI with optional leading loopback",
            ));
        }
        for plugin in &chain.2 {
            let kind = plugin["type"].as_str().unwrap();
            if !bin_dir.join(kind).is_file() {
                return Err(invalid(format!("missing VM CNI plugin {kind}")));
            }
            if kind == "bridge" {
                if plugin["ipam"]["type"] != "host-local" || !bin_dir.join("host-local").is_file() {
                    return Err(invalid("VM bridge requires the host-local IPAM plugin"));
                }
                // iproute2's main-table view cannot represent alternate tables,
                // route-specific MTUs/metrics or policy rules in the guest.
                // Reject those before CNI allocates or discards information.
                if let Some(routes) = plugin["ipam"].get("routes") {
                    let routes = routes
                        .as_array()
                        .ok_or_else(|| invalid("invalid CNI routes"))?;
                    for route in routes {
                        if !route
                            .as_object()
                            .is_some_and(|fields| fields.keys().all(|k| k == "dst" || k == "gw"))
                        {
                            return Err(invalid("unsupported VM CNI route attributes"));
                        }
                        ipv4_cidr(
                            route["dst"]
                                .as_str()
                                .ok_or_else(|| invalid("missing CNI route destination"))?,
                        )?;
                    }
                }
            }
        }
        fs::create_dir_all(path.parent().unwrap())?;
        let _lease = try_operation_lock(&path, id)?;
        fs::create_dir_all(&self.config.network.netns_dir)?;
        let netns = self
            .config
            .network
            .netns_dir
            .canonicalize()?
            .join(format!("vm-{id}"));
        match fs::symlink_metadata(&netns) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
            Ok(_) => return Err(invalid("VM namespace path is already occupied")),
        }
        let directory = netns.parent().unwrap();
        // An unnamed file has no crash window between creation and journaling
        // its inode. linkat publishes it only when the final path is unused.
        let placeholder = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_TMPFILE)
            .mode(0o600)
            .open(directory)?;
        let placeholder_meta = placeholder.metadata()?;
        // A short-lived OS thread creates an anonymous namespace. Its FD pins
        // the namespace while we journal its inode *before* publishing it.
        let handle = std::thread::spawn(|| -> std::io::Result<File> {
            nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNET)?;
            File::open("/proc/thread-self/ns/net")
        })
        .join()
        .map_err(|_| invalid("VM namespace worker failed"))??;
        let meta = handle.metadata()?;
        let reservation = Reservation {
            bin_dir,
            chain,
            netns,
            namespace_id: (meta.dev(), meta.ino()),
            placeholder_id: Some((placeholder_meta.dev(), placeholder_meta.ino())),
            adding: false,
            result: None,
        };
        saved(&path, &reservation, true)?;
        let name = std::ffi::CString::new(reservation.netns.as_os_str().as_bytes())
            .map_err(|_| invalid("invalid VM network path"))?;
        if unsafe {
            libc::linkat(
                placeholder.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_FDCWD,
                name.as_ptr(),
                libc::AT_EMPTY_PATH,
            )
        } == -1
        {
            return Err(std::io::Error::last_os_error().into());
        }
        mount(
            Some(Path::new(&format!("/proc/self/fd/{}", handle.as_raw_fd()))),
            &reservation.netns,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .map_err(|e| invalid(format!("publish VM network namespace: {e}")))?;
        Ok(PodNetInfo {
            netns_path: reservation.netns.to_string_lossy().into_owned(),
            holder_pid: 0,
            ip: String::new(),
        })
    }

    pub async fn attach_vm(
        &self,
        id: &str,
        info: &mut PodNetInfo,
        dns: Option<&DnsConfig>,
    ) -> Result<VmNetwork> {
        let path = self.vm_record(id)?;
        let lease = operation_lock(&path, id).await?;
        let mut record = read(&path)?;
        if record.adding
            || record.netns != Path::new(&info.netns_path)
            || record.mounted()? != Some(true)
        {
            return Err(invalid(
                "VM CNI reservation is incomplete, already used, or has changed identity",
            ));
        }
        record.adding = true;
        saved(&path, &record, false)?;
        let ns = &info.netns_path;
        let result = Self::run_chain(
            &record.bin_dir,
            record.chain.clone(),
            "ADD",
            id,
            ns,
            None,
            Some(&lease),
        )
        .await?;
        record.result = Some(result.clone());
        saved(&path, &record, false)?;
        let ips = result["ips"]
            .as_array()
            .ok_or_else(|| invalid("CNI returned no IPs"))?;
        if ips.len() != 1 {
            return Err(invalid("VM networking requires exactly one IPv4 address"));
        }
        let index = ips[0]["interface"]
            .as_u64()
            .ok_or_else(|| invalid("CNI IP has no interface"))? as usize;
        let interface = &result["interfaces"][index];
        if interface["name"] != "eth0" || interface["sandbox"].as_str() != Some(ns) {
            return Err(invalid("CNI Pod IP does not belong to sandbox eth0"));
        }
        let addr = ips[0]["address"]
            .as_str()
            .ok_or_else(|| invalid("missing CNI address"))?;
        let (ip, _) = ipv4_cidr(addr)?;
        let link = in_namespace(
            &lease,
            ns,
            &["ip", "-d", "-j", "link", "show", "dev", "eth0"],
        )
        .await?;
        let link: Vec<Value> = serde_json::from_slice(&link)?;
        let link = link.first().ok_or_else(|| invalid("missing eth0"))?;
        if link["linkinfo"]["info_kind"] != "veth" {
            return Err(invalid("VM bridge CNI must create a veth pair"));
        }
        let mac = link["address"]
            .as_str()
            .ok_or_else(|| invalid("missing CNI MAC"))?;
        if mac.len() != 17
            || mac.split(':').count() != 6
            || !mac
                .split(':')
                .all(|byte| byte.len() == 2 && byte.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(invalid("invalid veth MAC address"));
        }
        let mtu = link["mtu"]
            .as_u64()
            .filter(|m| (68..=65535).contains(m))
            .ok_or_else(|| invalid("invalid CNI MTU"))?;
        let routes = in_namespace(
            &lease,
            ns,
            &["ip", "-j", "-4", "route", "show", "dev", "eth0"],
        )
        .await?;
        let routes: Vec<Value> = serde_json::from_slice(&routes)?;
        let routes = guest_routes(&routes, addr)?;
        let resolv_conf = self.vm_resolver(dns, &result["dns"])?;
        let mut cmdline = format!("nautilus.net={addr}|{mtu}|{}|", routes.join(","));
        for byte in resolv_conf.bytes() {
            write!(cmdline, "{byte:02x}").unwrap();
        }
        // Leave room in Linux x86's 2048-byte command line for boot identity,
        // the share, and other fixed VMM arguments.
        if cmdline.len() > 1500 {
            return Err(invalid(
                "VM routes and DNS exceed the guest command-line limit",
            ));
        }

        in_namespace(
            &lease,
            ns,
            &["ip", "tuntap", "add", "nttap0", "mode", "tap"],
        )
        .await?;
        in_namespace(
            &lease,
            ns,
            &["ip", "link", "set", "nttap0", "mtu", &mtu.to_string(), "up"],
        )
        .await?;
        // CNI's namespace retains the endpoint name and MAC until DEL. Move
        // all ingress to the peer; only the guest should own the IPv4 address.
        for (from, to) in [("nttap0", "eth0"), ("eth0", "nttap0")] {
            in_namespace(&lease, ns, &["tc", "qdisc", "add", "dev", from, "clsact"]).await?;
            in_namespace(
                &lease,
                ns,
                &[
                    "tc", "filter", "add", "dev", from, "ingress", "protocol", "all", "matchall",
                    "action", "mirred", "egress", "redirect", "dev", to,
                ],
            )
            .await?;
        }
        in_namespace(
            &lease,
            ns,
            &["ip", "-4", "addr", "del", addr, "dev", "eth0"],
        )
        .await?;
        info.ip = ip.to_string();
        Ok(VmNetwork {
            netns_path: ns.clone(),
            mac: mac.to_owned(),
            cmdline,
            resolv_conf,
        })
    }

    /// Called after the VMM stops, including recovery of an interrupted
    /// reservation where CRI had not yet stored the namespace path.
    pub async fn teardown_vm(&self, id: &str) -> Result<()> {
        let path = self.vm_record(id)?;
        if !path.try_exists()? {
            return Ok(());
        }
        let lease = operation_lock(&path, id).await?;
        if !path.try_exists()? {
            return Ok(());
        }
        let mut record = read(&path)?;
        let mounted = record.mounted()? == Some(true);
        if record.adding {
            let ns = if mounted {
                record
                    .netns
                    .to_str()
                    .ok_or_else(|| invalid("invalid netns path"))?
            } else {
                ""
            };
            Self::run_chain(
                &record.bin_dir,
                record.chain.clone(),
                "DEL",
                id,
                ns,
                record.result.clone(),
                Some(&lease),
            )
            .await?;
            record.adding = false;
            saved(&path, &record, false)?;
        }
        if mounted {
            umount2(&record.netns, MntFlags::MNT_DETACH)
                .map_err(|e| invalid(format!("unmount VM namespace: {e}")))?;
        }
        match record.mounted()? {
            Some(false) => fs::remove_file(&record.netns)?,
            Some(true) => return Err(invalid("VM namespace remains mounted")),
            None => {}
        }
        fs::remove_file(&path)?;
        File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    }

    fn vm_resolver(&self, cri: Option<&DnsConfig>, cni: &Value) -> Result<String> {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Dns {
            nameservers: Vec<String>,
            domain: String,
            search: Vec<String>,
            options: Vec<String>,
        }
        let mut cni: Dns = if cni.is_null() {
            Dns::default()
        } else {
            serde_json::from_value(cni.clone()).map_err(|e| invalid(format!("CNI DNS: {e}")))?
        };
        if cni.search.is_empty() && !cni.domain.is_empty() {
            cni.search.push(cni.domain);
        }
        let default = &self.config.network.dns;
        let (servers, search, options) = match cri {
            Some(d) => (&d.servers, &d.searches, &d.options),
            None if !(cni.nameservers.is_empty()
                && cni.search.is_empty()
                && cni.options.is_empty()) =>
            {
                (&cni.nameservers, &cni.search, &cni.options)
            }
            None => (
                &default.nameservers,
                &default.search_domains,
                &default.options,
            ),
        };
        let mut out = String::new();
        for server in servers {
            let ip: Ipv4Addr = server
                .parse()
                .map_err(|_| invalid("VM DNS requires IPv4 nameservers"))?;
            writeln!(out, "nameserver {ip}").unwrap();
        }
        for (label, words) in [("search", search), ("options", options)] {
            if !words.is_empty() {
                if !words.iter().all(|w| {
                    !w.is_empty()
                        && w.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b".-_:".contains(&b))
                }) {
                    return Err(invalid(format!("invalid DNS {label} token")));
                }
                writeln!(out, "{label} {}", words.join(" ")).unwrap();
            }
        }
        Ok(out)
    }
}

fn ipv4_cidr(s: &str) -> Result<(Ipv4Addr, u8)> {
    let (ip, prefix) = s
        .split_once('/')
        .ok_or_else(|| invalid("missing IPv4 prefix"))?;
    Ok((
        ip.parse()
            .map_err(|_| invalid("VM networking requires IPv4"))?,
        prefix
            .parse()
            .ok()
            .filter(|p| *p <= 32)
            .ok_or_else(|| invalid("invalid IPv4 prefix"))?,
    ))
}

fn guest_routes(routes: &[Value], addr: &str) -> Result<Vec<String>> {
    let (ip, bits) = ipv4_cidr(addr)?;
    let connected = Ipv4Addr::from(u32::from(ip) & !u32::MAX.checked_shr(bits.into()).unwrap_or(0));
    let mut encoded = Vec::new();
    for route in routes {
        let dst = route["dst"]
            .as_str()
            .ok_or_else(|| invalid("missing IPv4 route destination"))?;
        let cidr = if dst == "default" {
            "0.0.0.0/0".into()
        } else if dst.contains('/') {
            dst.to_owned()
        } else {
            format!("{dst}/32")
        };
        let (dest, prefix) = ipv4_cidr(&cidr)?;
        if route["protocol"] == "kernel" && (dest, prefix) == (connected, bits) {
            continue;
        }
        let fields = route
            .as_object()
            .ok_or_else(|| invalid("invalid IPv4 route"))?;
        if !fields.keys().all(|k| {
            matches!(
                k.as_str(),
                "dst" | "gateway" | "dev" | "protocol" | "scope" | "flags"
            )
        }) || route["flags"].as_array().is_some_and(|f| !f.is_empty())
        {
            return Err(invalid("unsupported VM IPv4 route attributes"));
        }
        let mut entry = format!("{dest}/{prefix}");
        if let Some(gw) = route.get("gateway") {
            let ip: Ipv4Addr = gw
                .as_str()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| invalid("invalid IPv4 gateway"))?;
            write!(entry, "@{ip}").unwrap();
        }
        encoded.push(entry);
    }
    Ok(encoded)
}
