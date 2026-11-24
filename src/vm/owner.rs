//! Durable ownership of experimental per-Pod Cloud Hypervisor services.

use super::guest::canonical_uuid;
use serde::{Deserialize, Serialize};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::time::{sleep, timeout, Instant};
use uuid::Uuid;

const SOCKETS: [&str; 2] = ["api.sock", "vsock.sock"];
const UNIT_BUDGET: Duration = Duration::from_secs(8);
const BOOT_BUDGET: Duration = Duration::from_secs(20);
const MUTATION_LOCK_BUDGET: Duration = Duration::from_secs(45);
const RECOVERY_BUDGET: Duration = Duration::from_secs(12);
const STATUS_LOCK_BUDGET: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub struct PodVmStore {
    root: PathBuf,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn from_meta(meta: &fs::Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
        }
    }

    fn check(path: &Path) -> io::Result<Self> {
        let meta = fs::symlink_metadata(path)?;
        Self::directory(path, &meta)
    }

    fn directory(path: &Path, meta: &fs::Metadata) -> io::Result<Self> {
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!("insecure VM directory {}", path.display()),
            ));
        }
        Ok(Self::from_meta(meta))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Owner {
    id: String,
    nonce: String,
    store: FileIdentity,
    directory: Option<FileIdentity>,
    #[serde(default)]
    sockets: [Option<FileIdentity>; 2],
    invocation: Option<String>,
    boot: Option<String>,
    removing: bool,
}

impl Owner {
    fn unit(&self) -> String {
        format!("nautilus-vm-{}-{}.service", self.id, self.nonce)
    }

    fn description(&self) -> String {
        format!("Nautilus VM {} {}", self.id, self.nonce)
    }
}

struct Lease {
    root: PathBuf,
    directory: PathBuf,
    marker: PathBuf,
    draft: PathBuf,
    lock: File,
}

#[derive(Clone, Copy)]
enum Operation {
    Start,
    Status,
    Recover,
    Remove,
}

struct OnlineGuest {
    boot: String,
    invocation: String,
    sockets: [Option<FileIdentity>; 2],
}

struct Unit {
    invocation: String,
    state: String,
    pid: Option<i32>,
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, msg.into())
}

impl PodVmStore {
    /// The root is dedicated to this operator and created on first start.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    async fn lease(&self, id: &str, operation: Operation) -> io::Result<Option<Lease>> {
        let uuid = Uuid::parse_str(id).map_err(|_| invalid("invalid VM sandbox UUID"))?;
        if uuid.is_nil() || uuid.to_string() != id {
            return Err(invalid("expected a canonical VM sandbox UUID"));
        }
        let create = matches!(operation, Operation::Start);
        let read_only = matches!(operation, Operation::Status);
        if create {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.root)?;
        }
        match FileIdentity::check(&self.root) {
            Err(e) if !create && e.kind() == ErrorKind::NotFound => return Ok(None),
            result => {
                result?;
            }
        }
        let root = self.root.canonicalize()?;
        let marker = root.join(format!("{id}.json"));
        let lock = match OpenOptions::new()
            .read(true)
            .write(!read_only)
            .create(create)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(root.join(".operations"))
        {
            Err(e) if !create && e.kind() == ErrorKind::NotFound && !marker.try_exists()? => {
                return Ok(None)
            }
            result => result?,
        };
        // Independent processes lock the same byte for this Pod. A rare hash
        // collision only serializes two otherwise independent operations.
        let slot = (uuid.as_u128() as u64 % i64::MAX as u64) as i64;
        let mut range: libc::flock = unsafe { std::mem::zeroed() };
        range.l_type = if read_only {
            libc::F_RDLCK
        } else {
            libc::F_WRLCK
        } as _;
        range.l_whence = libc::SEEK_SET as _;
        range.l_start = slot;
        range.l_len = 1;
        let deadline = Instant::now()
            + match operation {
                Operation::Start | Operation::Remove => MUTATION_LOCK_BUDGET,
                Operation::Recover => RECOVERY_BUDGET,
                Operation::Status => STATUS_LOCK_BUDGET,
            };
        loop {
            if unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_OFD_SETLK, &range) } == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            if !matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EACCES)) {
                return Err(error);
            }
            if Instant::now() >= deadline {
                if read_only {
                    return Err(io::Error::new(
                        ErrorKind::WouldBlock,
                        "VM operation in progress",
                    ));
                }
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    format!("timed out locking sandbox {id}"),
                ));
            }
            sleep(Duration::from_millis(25)).await;
        }
        Ok(Some(Lease {
            directory: root.join(id),
            draft: root.join(format!(".{id}.next")),
            root,
            marker,
            lock,
        }))
    }

    pub async fn start(
        &self,
        id: &str,
        vmm: &Path,
        kernel: &Path,
        initramfs: &Path,
    ) -> io::Result<String> {
        let lease = self
            .lease(id, Operation::Start)
            .await?
            .expect("start creates the store");
        if let Some(owner) = lease.load(id)? {
            if owner.removing || owner.boot.is_none() {
                return Err(io::Error::other("VM attempt requires recovery"));
            }
            return lease.online_with_deadline(&owner).await?.ok_or_else(|| {
                io::Error::other("VM attempt has exited or is incomplete; remove before retrying")
            });
        }
        let vmm = vmm.canonicalize()?;
        let kernel = kernel.canonicalize()?;
        let initramfs = initramfs.canonicalize()?;
        let mut owner = Owner {
            id: id.into(),
            nonce: Uuid::new_v4().to_string(),
            store: FileIdentity::check(&lease.root)?,
            directory: None,
            sockets: [None, None],
            invocation: None,
            boot: None,
            removing: false,
        };
        // Confirm the system manager can be reached before recording intent.
        if lease.unit(&owner).await?.is_some() {
            return Err(invalid("VM unit already exists before launch"));
        }
        lease.save(&owner)?;
        // A canceled RPC drops this sender. Keep cleanup keyed to the nonce
        // so a queued cleanup can never stop a later attempt using this ID.
        let (finished, completed) = tokio::sync::oneshot::channel::<()>();
        let store = self.clone();
        let sandbox_id = id.to_owned();
        let nonce = owner.nonce.clone();
        tokio::spawn(async move {
            if completed.await.is_err() {
                if let Err(e) = store.remove_if(&sandbox_id, Some(&nonce)).await {
                    tracing::warn!(%sandbox_id, error = %e, "incomplete VM retained for cleanup");
                }
            }
        });
        let result = match lease.launch(&mut owner, &vmm, &kernel, &initramfs).await {
            Err(cause) => match lease.remove(&mut owner).await {
                Ok(()) => Err(cause),
                Err(cleanup) => Err(io::Error::new(
                    cleanup.kind(),
                    format!("{cause}; VM retained for cleanup: {cleanup}"),
                )),
            },
            ok => ok,
        };
        let _ = finished.send(());
        result
    }

    pub async fn status(&self, id: &str) -> io::Result<Option<String>> {
        let Some(lease) = self.lease(id, Operation::Status).await? else {
            return Ok(None);
        };
        let Some(owner) = lease.load(id)? else {
            return Ok(None);
        };
        if owner.removing || owner.boot.is_none() {
            return Err(io::Error::new(
                ErrorKind::WouldBlock,
                "VM startup or removal has not been reconciled",
            ));
        }
        lease.online_with_deadline(&owner).await
    }

    /// Reopen a committed guest or finish the abandoned operation under the same lease.
    pub async fn recover(&self, id: &str) -> io::Result<Option<String>> {
        timeout(RECOVERY_BUDGET, async {
            let Some(lease) = self.lease(id, Operation::Recover).await? else {
                return Ok(None);
            };
            lease.discard_draft()?;
            let Some(mut owner) = lease.load(id)? else {
                return Ok(None);
            };
            if !owner.removing && owner.boot.is_some() {
                if let Some(guest) = lease.online(&owner).await? {
                    if owner.invocation.is_none() || owner.sockets.contains(&None) {
                        return lease.commit_guest(&mut owner, guest).map(Some);
                    }
                    return Ok(Some(guest.boot));
                }
            }
            lease.remove(&mut owner).await?;
            Ok(None)
        })
        .await
        .map_err(|_| io::Error::new(ErrorKind::TimedOut, "VM recovery timed out"))?
    }

    pub async fn remove(&self, id: &str) -> io::Result<()> {
        self.remove_if(id, None).await
    }

    async fn remove_if(&self, id: &str, nonce: Option<&str>) -> io::Result<()> {
        let Some(lease) = self.lease(id, Operation::Remove).await? else {
            return Ok(());
        };
        let Some(mut owner) = lease.load(id)? else {
            if nonce.is_none() {
                lease.discard_draft()?;
            }
            return Ok(());
        };
        if nonce.is_some_and(|expected| owner.nonce != expected) {
            return Ok(());
        }
        lease.remove(&mut owner).await
    }
}

impl Lease {
    // The VMM binds relative paths. Pin its directory so host connects fit
    // sockaddr_un even when the operator configures a deeply nested state_dir.
    fn open_socket_directory(&self, owner: &Owner) -> io::Result<(File, PathBuf)> {
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(&self.directory)?;
        if Some(FileIdentity::directory(&self.directory, &dir.metadata()?)?) != owner.directory {
            return Err(invalid("VM socket directory identity changed"));
        }
        let path = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        Ok((dir, path))
    }

    fn discard_draft(&self) -> io::Result<()> {
        // The first marker write may have been interrupted before its atomic rename.
        match fs::symlink_metadata(&self.draft) {
            Ok(m) if m.is_file() && m.nlink() == 1 => fs::remove_file(&self.draft),
            Ok(_) => Err(invalid("unexpected provisional VM ownership record")),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn launch_token(&self, owner: &Owner) -> PathBuf {
        self.directory.join(format!(".launch-{}", owner.nonce))
    }

    fn load(&self, id: &str) -> io::Result<Option<Owner>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.marker)
        {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return match fs::symlink_metadata(&self.directory) {
                    Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e),
                    Ok(_) => Err(invalid("VM directory exists without an ownership record")),
                };
            }
            other => other?,
        };
        if !file.metadata()?.is_file() {
            return Err(invalid("VM ownership record is not a regular file"));
        }
        let owner: Owner = serde_json::from_reader(file).map_err(|e| invalid(e.to_string()))?;
        if owner.id != id
            || !canonical_uuid(&owner.nonce)
            || owner.store != FileIdentity::check(&self.root)?
        {
            return Err(invalid(
                "VM ownership record does not match this sandbox and store",
            ));
        }
        Ok(Some(owner))
    }

    fn save(&self, owner: &Owner) -> io::Result<()> {
        self.discard_draft()?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.draft)?;
        serde_json::to_writer(&mut file, owner)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&self.draft, &self.marker)?;
        File::open(&self.root)?.sync_all()
    }

    async fn submit(&self, mut cmd: Command) -> io::Result<Output> {
        let fd = self.lock.as_raw_fd();
        // A canceled client may still be submitting a unit job. Its inherited
        // OFD lock prevents another process racing that submission.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self::command(cmd).await
    }

    async fn command(mut cmd: Command) -> io::Result<Output> {
        timeout(UNIT_BUDGET, cmd.kill_on_drop(true).output())
            .await
            .map_err(|_| io::Error::new(ErrorKind::TimedOut, "systemd operation timed out"))?
    }

    fn command_error(action: &str, output: &Output) -> io::Error {
        io::Error::other(format!(
            "{action}: {} ({})",
            String::from_utf8_lossy(&output.stderr).trim(),
            output.status
        ))
    }

    async fn unit(&self, owner: &Owner) -> io::Result<Option<Unit>> {
        let mut cmd = Command::new("systemctl");
        cmd.args([
            "--system",
            "--no-pager",
            "--no-ask-password",
            "show",
            "--all",
            "-p",
            "LoadState,ActiveState,Transient,Description,WorkingDirectory,InvocationID,MainPID",
            &owner.unit(),
        ]);
        let output = Self::command(cmd).await?;
        self.parse_unit(owner, &output)
    }

    fn parse_unit(&self, owner: &Owner, output: &Output) -> io::Result<Option<Unit>> {
        let fields = String::from_utf8_lossy(&output.stdout);
        let prop = |key: &str| {
            fields
                .lines()
                .filter_map(|s| s.split_once('='))
                .find_map(|(k, v)| (k == key).then_some(v))
        };
        if prop("LoadState") == Some("not-found")
            && (output.status.success() || output.status.code() == Some(4))
        {
            return Ok(None);
        }
        if !output.status.success() {
            return Err(Self::command_error("query transient VM service", output));
        }
        if prop("LoadState") != Some("loaded")
            || prop("Transient") != Some("yes")
            || prop("Description") != Some(owner.description().as_str())
            || prop("WorkingDirectory").map(Path::new) != Some(self.directory.as_path())
        {
            return Err(invalid("systemd unit identity does not match VM owner"));
        }
        let invocation = prop("InvocationID").unwrap_or_default();
        if Uuid::parse_str(invocation).is_err()
            || owner
                .invocation
                .as_deref()
                .is_some_and(|expected| expected != invocation)
        {
            return Err(invalid(
                "systemd invocation identity does not match VM owner",
            ));
        }
        Ok(Some(Unit {
            invocation: invocation.into(),
            state: prop("ActiveState")
                .ok_or_else(|| invalid("missing unit state"))?
                .into(),
            pid: prop("MainPID")
                .and_then(|p| p.parse::<i32>().ok())
                .filter(|p| *p > 1),
        }))
    }

    async fn launch(
        &self,
        owner: &mut Owner,
        vmm: &Path,
        kernel: &Path,
        initramfs: &Path,
    ) -> io::Result<String> {
        // Journal the directory inode before publishing it at the canonical path.
        let staging = self.root.join(format!(".{}.work", owner.nonce));
        DirBuilder::new().mode(0o700).create(&staging)?;
        owner.directory = Some(FileIdentity::check(&staging)?);
        self.save(owner)?;
        nix::fcntl::renameat2(
            None,
            &staging,
            None,
            &self.directory,
            nix::fcntl::RenameFlags::RENAME_NOREPLACE,
        )
        .map_err(io::Error::from)?;
        File::open(&self.root)?.sync_all()?;
        let token = self.launch_token(owner);
        File::create_new(&token)?.sync_all()?;
        File::open(&self.directory)?.sync_all()?;
        let mut cmd = Command::new("systemd-run");
        cmd.args([
            "--system",
            "--quiet",
            "--no-ask-password",
            "--collect",
            "--service-type=exec",
            "--property=KillMode=control-group",
            "--property=TimeoutStopSec=2s",
            "--property=UMask=0077",
            "--unit",
            &owner.unit(),
            "--description",
            &owner.description(),
            "--working-directory",
        ])
        .arg(&self.directory)
        .arg("--property")
        .arg(format!("ConditionPathExists={}", token.display()))
        .arg("--")
        .arg(vmm)
        .arg("--kernel")
        .arg(kernel)
        .arg("--initramfs")
        .arg(initramfs)
        .args([
            "--cpus",
            "boot=1",
            "--memory",
            "size=384M",
            "--console",
            "off",
            "--serial",
            "off",
            "--api-socket",
            "api.sock",
            "--vsock",
            "cid=3,socket=vsock.sock",
            "--cmdline",
        ])
        .arg(format!(
            "rdinit=/init panic=1 nautilus.sandbox_id={} nautilus.nonce={}",
            owner.id, owner.nonce
        ));
        let output = self.submit(cmd).await?;
        if !output.status.success() {
            return Err(Self::command_error("start transient VM service", &output));
        }
        let unit = self
            .unit(owner)
            .await?
            .ok_or_else(|| io::Error::other("VMM exited during start"))?;
        owner.invocation = Some(unit.invocation);
        self.save(owner)?;
        timeout(BOOT_BUDGET, async {
            loop {
                match self.online(owner).await {
                    Ok(Some(guest)) => return self.commit_guest(owner, guest),
                    Ok(None) => {
                        return Err(io::Error::other("VMM exited before guest became available"))
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::NotFound
                                | ErrorKind::ConnectionRefused
                                | ErrorKind::ConnectionReset
                                | ErrorKind::UnexpectedEof
                                | ErrorKind::BrokenPipe
                                | ErrorKind::TimedOut
                        ) =>
                    {
                        sleep(Duration::from_millis(80)).await
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await
        .map_err(|_| io::Error::new(ErrorKind::TimedOut, "timed out identifying VM guest"))?
    }

    fn commit_guest(&self, owner: &mut Owner, guest: OnlineGuest) -> io::Result<String> {
        owner.boot = Some(guest.boot.clone());
        owner.invocation = Some(guest.invocation);
        owner.sockets = guest.sockets;
        self.save(owner)?;
        Ok(guest.boot)
    }

    async fn online_with_deadline(&self, owner: &Owner) -> io::Result<Option<String>> {
        let guest = timeout(UNIT_BUDGET, self.online(owner))
            .await
            .map_err(|_| io::Error::new(ErrorKind::TimedOut, "VM status timed out"))??;
        Ok(guest.map(|guest| guest.boot))
    }

    async fn online(&self, owner: &Owner) -> io::Result<Option<OnlineGuest>> {
        let Some(unit) = self.unit(owner).await? else {
            return Ok(None);
        };
        match unit.state.as_str() {
            "active" => {}
            "inactive" | "failed" => return Ok(None),
            _ => {
                return Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    "VM service is transitioning",
                ))
            }
        }
        let (_dir, socket_dir) = self.open_socket_directory(owner)?;
        let socket = socket_dir.join("vsock.sock");
        let mut sockets = [None, None];
        for (index, name) in SOCKETS.iter().enumerate() {
            let meta = fs::symlink_metadata(socket_dir.join(name))?;
            let identity = FileIdentity::from_meta(&meta);
            if !meta.file_type().is_socket()
                || owner.sockets[index].is_some_and(|saved| saved != identity)
            {
                return Err(invalid(format!("VM endpoint identity changed: {name}")));
            }
            sockets[index] = Some(identity);
        }
        let peer = UnixStream::connect(socket_dir.join("api.sock")).await?;
        if peer.peer_cred()?.pid().filter(|p| *p > 1) != unit.pid || unit.pid.is_none() {
            return Err(invalid(
                "API endpoint does not belong to the VMM systemd started",
            ));
        }
        drop(peer);
        let boot = crate::vm::guest::identify(&socket, &owner.id, &owner.nonce).await?;
        if !canonical_uuid(&boot) || owner.boot.as_deref().is_some_and(|saved| saved != boot) {
            return Err(invalid("guest boot identity changed"));
        }
        let confirmed = self
            .unit(owner)
            .await?
            .ok_or_else(|| io::Error::other("VMM exited during guest identification"))?;
        if confirmed.state != "active"
            || confirmed.invocation != unit.invocation
            || confirmed.pid != unit.pid
        {
            return Err(invalid("VMM changed during guest identification"));
        }
        Ok(Some(OnlineGuest {
            boot,
            invocation: unit.invocation,
            sockets,
        }))
    }

    async fn remove(&self, owner: &mut Owner) -> io::Result<()> {
        if !owner.removing {
            owner.removing = true;
            self.save(owner)?;
        }
        match FileIdentity::check(&self.directory) {
            Ok(actual) if Some(actual) == owner.directory => {
                // A delayed StartTransientUnit submission must not resurrect
                // this attempt after cleanup (or after the sandbox ID is reused).
                match fs::remove_file(self.launch_token(owner)) {
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    result => result?,
                }
                File::open(&self.directory)?.sync_all()?;
            }
            Ok(_) => {
                return Err(invalid(
                    "refusing to stop VM with replaced resource directory",
                ))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if self.unit(owner).await?.is_some() {
            let mut cmd = Command::new("systemctl");
            cmd.args([
                "--system",
                "--no-pager",
                "--no-ask-password",
                "stop",
                &owner.unit(),
            ]);
            let output = self.submit(cmd).await?;
            let remaining = self.unit(owner).await?;
            if remaining.as_ref().is_some_and(|u| u.state != "inactive") {
                return Err(Self::command_error("VM service stop unconfirmed", &output));
            }
        }
        self.cleanup_files(owner).await
    }

    async fn cleanup_files(&self, owner: &Owner) -> io::Result<()> {
        match self.open_socket_directory(owner) {
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
            Ok((_dir, socket_dir)) => {
                for (index, name) in SOCKETS.iter().enumerate() {
                    let socket = socket_dir.join(name);
                    let meta = match fs::symlink_metadata(&socket) {
                        Err(e) if e.kind() == ErrorKind::NotFound => continue,
                        result => result?,
                    };
                    if !meta.file_type().is_socket()
                        || owner.sockets[index]
                            .is_some_and(|saved| saved != FileIdentity::from_meta(&meta))
                    {
                        return Err(invalid(format!(
                            "refusing to unlink a replaced VM endpoint: {name}"
                        )));
                    }
                    // The service may have died before its endpoints were recorded.
                    // A listening socket after its unit is gone belongs to someone else.
                    match timeout(UNIT_BUDGET, UnixStream::connect(&socket)).await {
                        Ok(Err(e)) if e.kind() == ErrorKind::ConnectionRefused => {
                            fs::remove_file(&socket)?
                        }
                        Ok(Err(e)) if e.kind() == ErrorKind::NotFound => {}
                        Ok(Err(e)) => return Err(e),
                        Ok(Ok(_)) => {
                            return Err(io::Error::new(
                                ErrorKind::WouldBlock,
                                format!("VM endpoint is still listening: {name}"),
                            ))
                        }
                        Err(_) => {
                            return Err(io::Error::new(
                                ErrorKind::TimedOut,
                                format!("timed out checking VM endpoint: {name}"),
                            ))
                        }
                    }
                }
                fs::remove_dir(&self.directory)?;
            }
        }
        // A crash during launch may have left only the private staging directory.
        let staging = self.root.join(format!(".{}.work", owner.nonce));
        match FileIdentity::check(&staging) {
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
            Ok(found) if owner.directory.is_none_or(|expected| expected == found) => {
                fs::remove_dir(staging)?
            }
            Ok(_) => return Err(invalid("VM staging directory identity changed")),
        }
        fs::remove_file(&self.marker)?;
        File::open(&self.root)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    const ID: &str = "d07de0d1-b5a4-4082-8c32-b7d400364f96";

    async fn fixture() -> (tempfile::TempDir, PodVmStore, Lease, Owner) {
        fixture_at("store").await
    }

    async fn fixture_at(name: &str) -> (tempfile::TempDir, PodVmStore, Lease, Owner) {
        let work = tempfile::tempdir().unwrap();
        let store = PodVmStore::new(work.path().join(name));
        let lease = store.lease(ID, Operation::Start).await.unwrap().unwrap();
        DirBuilder::new()
            .mode(0o700)
            .create(&lease.directory)
            .unwrap();
        let owner = Owner {
            id: ID.into(),
            nonce: Uuid::new_v4().to_string(),
            store: FileIdentity::check(&lease.root).unwrap(),
            directory: Some(FileIdentity::check(&lease.directory).unwrap()),
            sockets: [None, None],
            invocation: Some(Uuid::new_v4().simple().to_string()),
            boot: None,
            removing: false,
        };
        lease.save(&owner).unwrap();
        (work, store, lease, owner)
    }

    async fn finish_cleanup(lease: &Lease, owner: &Owner) {
        // Other parallel tests can fork while this test's listener is open.
        // Cleanup must wait for the last inherited descriptor to close.
        timeout(Duration::from_secs(3), async {
            loop {
                match lease.cleanup_files(owner).await {
                    Ok(()) => break,
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::WouldBlock | ErrorKind::ConnectionReset
                        ) =>
                    {
                        sleep(Duration::from_millis(25)).await;
                    }
                    Err(e) => panic!("unexpected cleanup error: {e}"),
                }
            }
        })
        .await
        .expect("test endpoint remained live after its listener was dropped");
    }

    #[tokio::test]
    async fn missing_vm_operations_do_not_materialize_store() {
        let work = tempfile::tempdir().unwrap();
        let root = work.path().join("missing");
        let store = PodVmStore::new(root.clone());
        assert_eq!(store.status(ID).await.unwrap(), None);
        assert_eq!(store.recover(ID).await.unwrap(), None);
        store.remove(ID).await.unwrap();
        assert!(store.remove("../escape").await.is_err());
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn status_returns_promptly_and_mutations_serialize_with_an_unpublished_start() {
        let work = tempfile::tempdir().unwrap();
        let store = PodVmStore::new(work.path().join("store"));
        let start = store.lease(ID, Operation::Start).await.unwrap();
        assert_eq!(
            store.status(ID).await.unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        assert!(timeout(Duration::from_millis(100), store.remove(ID))
            .await
            .is_err());
        drop(start);
        store.remove(ID).await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_status_readers_share_the_lease_and_exclude_removal() {
        let work = tempfile::tempdir().unwrap();
        let store = PodVmStore::new(work.path().join("store"));
        let writer = store.lease(ID, Operation::Start).await.unwrap().unwrap();
        drop(writer);
        let reader = store.lease(ID, Operation::Status).await.unwrap().unwrap();
        let (first, second) = tokio::join!(store.status(ID), store.status(ID));
        assert!(first.unwrap().is_none() && second.unwrap().is_none());
        assert!(timeout(
            Duration::from_millis(75),
            store.lease(ID, Operation::Remove)
        )
        .await
        .is_err());
        drop(reader);
        assert!(store.lease(ID, Operation::Remove).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn status_waits_for_a_briefly_inherited_writer_descriptor() {
        let work = tempfile::tempdir().unwrap();
        let store = PodVmStore::new(work.path().join("store"));
        let writer = store.lease(ID, Operation::Start).await.unwrap().unwrap();
        let inherited = writer.lock.try_clone().unwrap();
        drop(writer);
        let release = async {
            sleep(Duration::from_millis(50)).await;
            drop(inherited);
        };
        let ((), status) = tokio::join!(release, store.status(ID));
        assert!(status.unwrap().is_none());
    }

    #[tokio::test]
    async fn recovery_discards_a_partial_first_marker() {
        let work = tempfile::tempdir().unwrap();
        let store = PodVmStore::new(work.path().join("store"));
        let lease = store.lease(ID, Operation::Start).await.unwrap().unwrap();
        let path = lease.draft.clone();
        fs::write(&path, b"{").unwrap();
        drop(lease);
        assert_eq!(store.recover(ID).await.unwrap(), None);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn missing_marker_does_not_authorize_removing_a_resource_directory() {
        let (_work, store, lease, _) = fixture().await;
        fs::remove_file(&lease.marker).unwrap();
        let directory = lease.directory.clone();
        drop(lease);
        assert_eq!(
            store.recover(ID).await.unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(
            store.remove(ID).await.unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert!(directory.exists());
    }

    #[tokio::test]
    async fn status_and_start_cannot_commit_an_interrupted_attempt() {
        let (_work, store, lease, mut owner) = fixture().await;
        assert!(owner.boot.is_none());
        drop(lease);
        assert_eq!(
            store.status(ID).await.unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        let unused = Path::new("/unused");
        assert!(store.start(ID, unused, unused, unused).await.is_err());

        // Recording a removal intent does not prove the service has stopped.
        let lease = store.lease(ID, Operation::Remove).await.unwrap().unwrap();
        owner.boot = Some(Uuid::new_v4().to_string());
        owner.removing = true;
        lease.save(&owner).unwrap();
        drop(lease);
        assert_eq!(
            store.status(ID).await.unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
    }

    #[tokio::test]
    async fn unit_replacement_is_rejected_even_when_pid_is_reused() {
        let (_work, _store, lease, owner) = fixture().await;
        let parse = |invocation: &str, directory: &Path| {
            let fields = format!("LoadState=loaded\nActiveState=active\nMainPID=217\nTransient=yes\nDescription={}\nWorkingDirectory={}\nInvocationID={invocation}\n", owner.description(), directory.display());
            lease.parse_unit(
                &owner,
                &Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout: fields.into_bytes(),
                    stderr: vec![],
                },
            )
        };
        assert_eq!(
            parse(owner.invocation.as_deref().unwrap(), &lease.directory)
                .unwrap()
                .unwrap()
                .pid,
            Some(217)
        );
        assert!(parse(&Uuid::new_v4().simple().to_string(), &lease.directory).is_err());
        assert!(parse(owner.invocation.as_deref().unwrap(), &lease.root).is_err());
    }

    #[tokio::test]
    async fn cleanup_preserves_foreign_socket_and_can_be_retried() {
        let (_work, _store, lease, mut owner) = fixture().await;
        let socket = lease.directory.join("api.sock");
        let original = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        owner.sockets[0] = Some(FileIdentity::from_meta(
            &fs::symlink_metadata(&socket).unwrap(),
        ));
        let original_path = lease.directory.join("original");
        fs::rename(&socket, &original_path).unwrap();
        let replacement = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(lease.cleanup_files(&owner).await.is_err());
        assert!(socket.exists() && lease.marker.exists());
        drop(replacement);
        fs::remove_file(&socket).unwrap();
        fs::rename(original_path, &socket).unwrap();
        drop(original);
        finish_cleanup(&lease, &owner).await;
        assert!(!lease.directory.exists() && !lease.marker.exists());
    }

    #[tokio::test]
    async fn long_state_directory_supports_socket_connections_and_cleanup() {
        let (_work, _store, lease, owner) = fixture_at(&"s".repeat(120)).await;
        assert!(
            std::os::unix::net::SocketAddr::from_pathname(lease.directory.join("api.sock"))
                .is_err()
        );
        let (_dir, socket_dir) = lease.open_socket_directory(&owner).unwrap();
        let api = socket_dir.join("api.sock");
        let listener = std::os::unix::net::UnixListener::bind(&api).unwrap();
        let peer = UnixStream::connect(&api).await.unwrap();
        assert_eq!(
            peer.peer_cred().unwrap().pid(),
            Some(std::process::id() as i32)
        );
        drop(listener);
        finish_cleanup(&lease, &owner).await;
        assert!(!lease.directory.exists() && !lease.marker.exists());
    }
}
