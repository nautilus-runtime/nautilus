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
    mount: u64,
    source: FileIdentity,
    placeholder: FileIdentity,
}

pub(super) const STAGE: &str = ".volume-stage";

fn remove_slot(dir: &File, name: &str, expected: Option<FileIdentity>) -> io::Result<()> {
    let slot = match open_slot(dir, name) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        result => result?,
    };
    let meta = slot.metadata()?;
    if mount_id(dir)? != mount_id(&slot)?
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || expected.is_some_and(|id| id != FileIdentity::from_meta(&meta))
        || (!meta.is_dir() && (!meta.is_file() || meta.nlink() != 1 || meta.len() != 0))
    {
        return Err(invalid("refusing to remove a replaced volume placeholder"));
    }
    let path = pinned(dir).join(name);
    if meta.is_dir() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

// Only runtime-created empty placeholders live here. The guest cannot access
// this directory. An interrupted allocation can leave an unpublished placeholder
// before its receipt exists. Such a file may be reclaimed without consulting or
// touching the same name in the public catalog.
fn reap_stage(stage: &File, keep: &[Binding]) -> io::Result<()> {
    for entry in fs::read_dir(pinned(stage))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .filter(|s| canonical_uuid(s))
            .ok_or_else(|| invalid("unexpected entry in private volume staging"))?;
        if !keep.iter().any(|b| b.id == name) {
            remove_slot(stage, name, None)?;
        }
    }
    stage.sync_all()
}

pub(super) fn cleanup_stage(control: &Path, expected: Option<FileIdentity>) -> io::Result<()> {
    let path = control.join(STAGE);
    if expected.is_some() {
        match pin_directory(&path, expected) {
            Ok(stage) => reap_stage(&stage, &[])?,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    match fs::remove_dir(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

impl Binding {
    fn unpublished(&self, stage: &File) -> io::Result<bool> {
        match open_slot(stage, &self.id) {
            Ok(file)
                if FileIdentity::from_meta(&file.metadata()?) == self.placeholder
                    && mount_id(&file)? == mount_id(stage)? =>
            {
                Ok(true)
            }
            Ok(_) => Err(invalid("private volume placeholder changed")),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

pub(super) fn mount_id(file: &File) -> io::Result<u64> {
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

pub(super) fn open_slot(parent: &File, name: &str) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
        .open(pinned(parent).join(name))
}

pub(super) struct Worker {
    namespace: File,
    pub(super) root: File,
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
            root,
            catalog,
            process,
        }))
    }

    pub(super) fn in_namespace<T: Send>(
        &self,
        run: impl FnOnce() -> io::Result<T> + Send,
    ) -> io::Result<T> {
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

    pub(super) fn attach(&self, tree: &File, slot: &File) -> io::Result<()> {
        self.in_namespace(|| {
            LinuxSyscall
                .move_mount(
                    tree.as_fd(),
                    None,
                    slot.as_fd(),
                    None,
                    linux::MOVE_MOUNT_F_EMPTY_PATH | linux::MOVE_MOUNT_T_EMPTY_PATH,
                )
                .map_err(|e| io::Error::other(format!("publish mount in virtiofsd: {e}")))
        })
    }

    pub(super) fn has_mount(&self, id: u64) -> io::Result<bool> {
        let info = fs::read_to_string(pinned(&self.process).join("mountinfo"))?;
        Ok(info
            .lines()
            .any(|l| l.split_once(' ').and_then(|(id, _)| id.parse::<u64>().ok()) == Some(id)))
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
            if self.has_mount(binding.mount)? {
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
    pub(super) async fn volume_worker(&self, owner: &Owner) -> io::Result<Option<Worker>> {
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
        if share.volume_stage.is_none() {
            return Ok(());
        } // Older Pods.
        let (_directory, control) = self.open_socket_directory(owner)?;
        let stage = pin_directory(&control.join(STAGE), share.volume_stage)?;
        if !share.bindings.iter().any(|b| b.container == container) {
            return reap_stage(&stage, &share.bindings);
        }
        let worker = self.volume_worker(owner).await?;
        let host = self.host_catalog(owner)?;
        for b in share.bindings.iter().filter(|b| b.container == container) {
            if !canonical_uuid(&b.id) {
                return Err(invalid("invalid volume receipt"));
            }
            if b.unpublished(&stage)? {
                // RENAME_NOREPLACE has never succeeded. The public name may
                // already belong to someone else. Keep the staging witness
                // until the receipt removal is durable.
                continue;
            }
            if let Some(w) = &worker {
                w.detach(b)?;
            }
            remove_slot(&host, &b.id, Some(b.placeholder))?;
        }
        host.sync_all()?;
        owner
            .share
            .as_mut()
            .unwrap()
            .bindings
            .retain(|b| b.container != container);
        self.save(owner)?;
        reap_stage(&stage, &owner.share.as_ref().unwrap().bindings)
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
        let (_directory, control) = lease.open_socket_directory(&owner)?;
        let share = owner.share.as_mut().unwrap();
        let stage = pin_directory(&control.join(STAGE), share.volume_stage)?;
        reap_stage(&stage, &share.bindings)?;
        let staged = pinned(&stage).join(&id);
        if meta.is_dir() {
            DirBuilder::new().mode(0o700).create(&staged)?;
        } else {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&staged)?
                .sync_all()?;
        }
        stage.sync_all()?;
        let placeholder = FileIdentity::from_meta(&fs::symlink_metadata(&staged)?);
        share.bindings.push(Binding {
            container: container.into(),
            id: id.clone(),
            source: FileIdentity::from_meta(&meta),
            mount: mount_id(&tree)?,
            placeholder,
        });
        lease.save(&owner)?;
        nix::fcntl::renameat2(
            Some(stage.as_raw_fd()),
            Path::new(&id),
            Some(host.as_raw_fd()),
            Path::new(&id),
            nix::fcntl::RenameFlags::RENAME_NOREPLACE,
        )?;
        host.sync_all()?;
        stage.sync_all()?;
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
