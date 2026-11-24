//! Bind CRI-authorized inodes into the individual virtiofsd mount namespace.
//! Files on the node under podfs/volumes are always empty placeholders.
use super::share::{pin_directory, pinned};
use super::*;
use libcontainer::syscall::linux::{self, LinuxSyscall, MountAttr};
use libcontainer::syscall::Syscall;
use nix::mount::{umount2, MntFlags};
use nix::sched::{setns, unshare, CloneFlags};
use std::os::fd::AsFd;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Binding {
    container: String,
    id: String,
    is_dir: bool,
    mount: u64,
    source: FileIdentity,
    placeholder: Option<FileIdentity>,
}

fn mount_id(file: &File) -> io::Result<u64> {
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:")?.trim().parse().ok())
        .ok_or_else(|| invalid("mount identity unavailable"))
}

// Clone the selected view, including mounted descendants already visible to the
// CRI caller. A plain bind would instead reveal the hidden directories beneath
// those mounts. Make the detached tree private before publishing it.
fn detached(source: &File, readonly: bool, recursive: bool) -> io::Result<File> {
    let children = if recursive { linux::AT_RECURSIVE } else { 0 };
    let tree = LinuxSyscall
        .open_tree(
            source.as_raw_fd(),
            None,
            libc::OPEN_TREE_CLONE | libc::OPEN_TREE_CLOEXEC | linux::AT_EMPTY_PATH | children,
        )
        .map_err(|e| io::Error::other(format!("open detached volume: {e}")))?;
    let mut attrs = MountAttr {
        attr_set: linux::MOUNT_ATTR_NODEV | linux::MOUNT_ATTR_NOSUID | linux::MOUNT_ATTR_NOEXEC,
        attr_clr: 0,
        propagation: libc::MS_PRIVATE,
        userns_fd: 0,
    };
    if readonly {
        attrs.attr_set |= linux::MOUNT_ATTR_RDONLY;
    }
    LinuxSyscall
        .mount_setattr(
            tree.as_fd(),
            Path::new(""),
            linux::AT_EMPTY_PATH | children,
            &attrs,
            std::mem::size_of_val(&attrs),
        )
        .map_err(|e| io::Error::other(format!("set detached volume attributes: {e}")))?;
    Ok(tree.into())
}

fn open_slot(parent: &File, name: &str) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
        .open(pinned(parent).join(name))
}

pub(super) struct Worker {
    namespace: File,
    catalog: File,
    process: File,
}

impl Worker {
    pub(super) fn open(pid: i32, share: &Share) -> io::Result<Option<Self>> {
        // Pin this proc directory before resolving root/ns. Once this process
        // exits the fd cannot refer to another task that recycled its PID.
        let process = File::open(format!("/proc/{pid}"))?;
        let ns = File::open(pinned(&process).join("ns/mnt"))?;
        let root = File::open(pinned(&process).join("root"))?;
        if FileIdentity::from_meta(&ns.metadata()?)
            == FileIdentity::from_meta(&fs::metadata("/proc/self/ns/mnt")?)
            || Some(FileIdentity::from_meta(&root.metadata()?)) != share.export
        {
            return Ok(None); // virtiofsd has not entered the sandbox yet.
        }
        let catalog = pin_directory(&pinned(&root).join("volumes"), share.volumes)?;
        if let Some(expected) = share.volume_catalog_mount {
            if mount_id(&catalog)? != expected {
                return Err(invalid("virtiofsd volume catalog mount changed"));
            }
        }
        Ok(Some(Self {
            namespace: ns,
            catalog,
            process,
        }))
    }

    fn in_namespace<T: Send>(&self, run: impl FnOnce() -> io::Result<T> + Send) -> io::Result<T> {
        // Tokio's executor and other Pods keep their filesystem context.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    unshare(CloneFlags::CLONE_FS)?;
                    setns(&self.namespace, CloneFlags::CLONE_NEWNS)?;
                    run()
                })
                .join()
                .map_err(|_| io::Error::other("volume mount worker panicked"))?
        })
    }

    fn attach(&self, tree: &File, slot: &File) -> io::Result<()> {
        self.in_namespace(|| {
            LinuxSyscall
                .move_mount(
                    tree.as_fd(),
                    None,
                    slot.as_fd(),
                    None,
                    linux::MOVE_MOUNT_F_EMPTY_PATH | linux::MOVE_MOUNT_T_EMPTY_PATH,
                )
                .map_err(|e| io::Error::other(format!("publish volume in virtiofsd: {e}")))
        })
    }

    pub(super) fn seal_catalog(&self, host: &File) -> io::Result<u64> {
        // The guest cannot replace or rename a mountpoint in this read-only
        // directory. The node keeps its own writable view for creating slots.
        let tree = detached(host, true, false)?;
        let id = mount_id(&tree)?;
        self.attach(&tree, &self.catalog)?;
        Ok(id)
    }

    fn detach(&self, binding: &Binding) -> io::Result<()> {
        let slot = match open_slot(&self.catalog, &binding.id) {
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            result => Some(result?),
        };
        let actual = slot.as_ref().map(mount_id).transpose()?;
        if actual.is_none() || actual == Some(mount_id(&self.catalog)?) {
            // Missing here may also mean the name was replaced or renamed by
            // the host. Don't drop the receipt while our mount survives elsewhere.
            let info = fs::read_to_string(pinned(&self.process).join("mountinfo"))?;
            if info.lines().any(|l| {
                l.split_once(' ').and_then(|(id, _)| id.parse::<u64>().ok()) == Some(binding.mount)
            }) {
                return Err(invalid("volume moved away from its recorded placeholder"));
            }
            return Ok(());
        }
        let slot = slot.expect("present mount");
        if actual != Some(binding.mount)
            || FileIdentity::from_meta(&slot.metadata()?) != binding.source
        {
            return Err(invalid("refusing to detach a replaced volume"));
        }
        self.in_namespace(|| {
            nix::unistd::fchdir(self.catalog.as_raw_fd())?;
            umount2(
                Path::new(&binding.id),
                MntFlags::MNT_DETACH | MntFlags::UMOUNT_NOFOLLOW,
            )?;
            Ok(())
        })
    }
}

impl Lease {
    async fn volume_worker(&self, owner: &Owner) -> io::Result<Option<Worker>> {
        let share = owner
            .share
            .as_ref()
            .ok_or_else(|| invalid("Pod has no export"))?;
        if share.volume_catalog_mount.is_none() {
            return Err(invalid("recreate Pod to enable live volumes"));
        }
        let Some(unit) = self.unit(owner, Service::Virtiofsd).await? else {
            return Ok(None);
        };
        let Some(pid) = unit.pid else {
            if matches!(unit.state.as_str(), "inactive" | "failed") {
                return Ok(None);
            }
            return Err(io::Error::new(
                ErrorKind::WouldBlock,
                "virtiofsd is transitioning",
            ));
        };
        let worker =
            Worker::open(pid, share)?.ok_or_else(|| invalid("virtiofsd sandbox changed"))?;
        Ok(Some(worker))
    }

    fn host_catalog(&self, owner: &Owner) -> io::Result<File> {
        let share = owner
            .share
            .as_ref()
            .ok_or_else(|| invalid("Pod has no export"))?;
        let (_dir, control) = self.open_socket_directory(owner)?;
        let export = pin_directory(&control.join("podfs"), share.export)?;
        pin_directory(&pinned(&export).join("volumes"), share.volumes)
    }

    pub(super) async fn remove_volumes(
        &self,
        owner: &mut Owner,
        container: &str,
    ) -> io::Result<()> {
        let Some(share) = &owner.share else {
            return Ok(());
        };
        if !share.bindings.iter().any(|b| b.container == container) {
            return Ok(());
        }
        let worker = self.volume_worker(owner).await?;
        let host = self.host_catalog(owner)?;
        for b in share.bindings.iter().filter(|b| b.container == container) {
            if !canonical_uuid(&b.id) {
                return Err(invalid("invalid volume receipt"));
            }
            if let Some(w) = &worker {
                w.detach(b)?;
            }
            let slot = match open_slot(&host, &b.id) {
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                other => other?,
            };
            let meta = slot.metadata()?;
            if mount_id(&host)? != mount_id(&slot)?
                || b.placeholder
                    .is_some_and(|v| v != FileIdentity::from_meta(&meta))
                || (b.is_dir && !meta.is_dir())
                || (!b.is_dir && (!meta.is_file() || meta.nlink() != 1 || meta.len() != 0))
            {
                return Err(invalid("refusing to remove a replaced volume placeholder"));
            }
            let path = pinned(&host).join(&b.id);
            if b.is_dir {
                fs::remove_dir(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
        host.sync_all()?;
        owner
            .share
            .as_mut()
            .unwrap()
            .bindings
            .retain(|b| b.container != container);
        self.save(owner)
    }
}

impl PodVmStore {
    /// Return the opaque source selector for one OCI mount. Source pathname
    /// symlinks are followed once, as required by CRI. No host path enters the guest.
    pub async fn bind_volume(
        &self,
        pod: &str,
        container: &str,
        source: &Path,
        readonly: bool,
    ) -> io::Result<String> {
        if !canonical_uuid(container) || !source.is_absolute() {
            return Err(invalid(
                "expected container UUID and absolute volume source",
            ));
        }
        let missing = || io::Error::new(ErrorKind::NotFound, "VM attempt not found");
        let lease = self
            .lease(pod, Operation::Remove)
            .await?
            .ok_or_else(missing)?;
        let mut owner = lease.load(pod)?.ok_or_else(missing)?;
        if owner.removing || owner.boot.is_none() {
            return Err(invalid("VM is not accepting volumes"));
        }
        let worker = lease.volume_worker(&owner).await?.ok_or_else(missing)?;
        let host = lease.host_catalog(&owner)?;
        let source = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH)
            .open(source)?;
        let meta = source.metadata()?;
        if !meta.is_dir() && !meta.is_file() {
            return Err(invalid("volume must be a file or directory"));
        }
        let tree = detached(&source, readonly, meta.is_dir())?;
        let id = Uuid::new_v4().to_string();
        let binding = Binding {
            container: container.into(),
            id: id.clone(),
            is_dir: meta.is_dir(),
            source: FileIdentity::from_meta(&meta),
            mount: mount_id(&tree)?,
            placeholder: None,
        };
        owner.share.as_mut().unwrap().bindings.push(binding);
        lease.save(&owner)?;
        let slot = pinned(&host).join(&id);
        if meta.is_dir() {
            DirBuilder::new().mode(0o700).create(&slot)?;
        } else {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&slot)?
                .sync_all()?;
        }
        host.sync_all()?;
        let placeholder = FileIdentity::from_meta(&fs::symlink_metadata(&slot)?);
        owner
            .share
            .as_mut()
            .unwrap()
            .bindings
            .last_mut()
            .unwrap()
            .placeholder = Some(placeholder);
        lease.save(&owner)?;
        let target = open_slot(&worker.catalog, &id)?;
        if FileIdentity::from_meta(&target.metadata()?) != placeholder
            || mount_id(&target)? != mount_id(&worker.catalog)?
        {
            return Err(invalid("volume placeholder changed before attachment"));
        }
        worker.attach(&tree, &target)?;
        Ok(format!("volumes/{id}"))
    }
}
