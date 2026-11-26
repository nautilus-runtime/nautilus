//! A private virtiofsd and writable OCI bundles for one Pod attempt.

use super::*;
use nix::mount::{umount2, MntFlags};
use volume::{mount_id, open_slot, Worker};

const EXPORT: &str = "podfs";
const SOCKET: &str = "fs.sock";
const PID_FILE: &str = "fs.sock.pid";

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct RootfsMount {
    container: String,
    bundle: FileIdentity,
    placeholder: FileIdentity,
    root: FileIdentity,
    mount: u64,
}

impl RootfsMount {
    fn worker_bundle(&self, worker: &Worker, share: &Share) -> io::Result<File> {
        let bundles = pin_directory(&pinned(&worker.root).join("containers"), share.containers)?;
        pin_directory(&pinned(&bundles).join(&self.container), Some(self.bundle))
    }

    fn attach(&self, worker: &Worker, share: &Share, view: &File) -> io::Result<()> {
        let bundle = self.worker_bundle(worker, share)?;
        let slot = open_slot(&bundle, "rootfs")?;
        if FileIdentity::from_meta(&slot.metadata()?) != self.placeholder
            || mount_id(&slot)? != mount_id(&bundle)?
        {
            return Err(invalid("VM rootfs placeholder changed before attachment"));
        }
        worker.attach(view, &slot)
    }

    fn detach(&self, worker: &Worker, share: &Share) -> io::Result<()> {
        if !worker.has_mount(self.mount)? {
            return Ok(());
        }
        let bundle = self.worker_bundle(worker, share)?;
        let slot = open_slot(&bundle, "rootfs")?;
        if mount_id(&slot)? != self.mount || FileIdentity::from_meta(&slot.metadata()?) != self.root
        {
            return Err(invalid("refusing to detach a replaced VM rootfs"));
        }
        worker.in_namespace(|| {
            nix::unistd::fchdir(bundle.as_raw_fd())?;
            umount2("rootfs", MntFlags::MNT_DETACH | MntFlags::UMOUNT_NOFOLLOW)?;
            Ok(())
        })
    }
}

fn no_host_mounts(dir: &Path) -> io::Result<()> {
    crate::storage::snapshot::check_unmounted(&dir.canonicalize()?).map_err(io::Error::other)
}

fn endpoint(path: &Path, socket: bool, expected: Option<FileIdentity>) -> io::Result<FileIdentity> {
    let meta = fs::symlink_metadata(path)?;
    let identity = FileIdentity::from_meta(&meta);
    let kind_matches = if socket {
        meta.file_type().is_socket()
    } else {
        meta.is_file() && meta.nlink() == 1
    };
    if !kind_matches || expected.is_some_and(|saved| saved != identity) {
        return Err(invalid(format!(
            "virtiofsd endpoint identity changed: {}",
            path.display()
        )));
    }
    Ok(identity)
}

pub(super) fn pin_directory(path: &Path, expected: Option<FileIdentity>) -> io::Result<File> {
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    if Some(FileIdentity::from_meta(&dir.metadata()?)) != expected {
        return Err(invalid("Pod export directory identity changed"));
    }
    Ok(dir)
}

pub(super) fn pinned(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}

impl Lease {
    pub(super) async fn start_share(
        &self,
        owner: &mut Owner,
        executable: &Path,
        slice: Option<&str>,
    ) -> io::Result<()> {
        let export = self.directory.join(EXPORT);
        let containers = export.join("containers");
        DirBuilder::new().mode(0o700).create(&export)?;
        DirBuilder::new().mode(0o700).create(&containers)?;
        let stage = self.directory.join(volume::STAGE);
        DirBuilder::new().mode(0o700).create(&stage)?;
        let volumes = export.join("volumes");
        DirBuilder::new().mode(0o700).create(&volumes)?;
        let share = owner.share.as_mut().expect("share requested");
        share.volumes = Some(FileIdentity::check(&volumes)?);
        share.volume_stage = Some(FileIdentity::check(&stage)?);
        share.export = Some(FileIdentity::check(&export)?);
        share.containers = Some(FileIdentity::check(&containers)?);
        File::open(&export)?.sync_all()?;
        File::open(&self.directory)?.sync_all()?;
        self.save(owner)?;
        let mut cmd = self.service_command(owner, Service::Virtiofsd);
        if let Some(slice) = slice {
            cmd.arg("--slice").arg(slice);
        }
        cmd.arg("--").arg(executable).args([
            "--socket-path",
            SOCKET,
            "--shared-dir",
            EXPORT,
            "--xattr",
        ]);
        let output = self.submit(cmd).await?;
        if !output.status.success() {
            return Err(Self::command_error("start virtiofsd", &output));
        }
        timeout(UNIT_BUDGET, async {
            loop {
                let unit = self.unit(owner, Service::Virtiofsd).await?.ok_or_else(|| {
                    io::Error::other("virtiofsd exited before creating its socket")
                })?;
                let share = owner.share.as_mut().expect("share requested");
                match self.check_share_endpoints(share) {
                    Ok((socket, pid_file)) => {
                        let Some(pid) = unit.pid else {
                            sleep(Duration::from_millis(40)).await;
                            continue;
                        };
                        // The socket can exist before virtiofsd pivots into its export.
                        let Some(worker) = volume::Worker::open(pid, share)? else {
                            sleep(Duration::from_millis(40)).await;
                            continue;
                        };
                        share.volume_catalog_mount =
                            Some(worker.seal_catalog(&pin_directory(&volumes, share.volumes)?)?);
                        share.invocation = Some(unit.invocation);
                        share.socket = Some(socket);
                        share.pid_file = Some(pid_file);
                        return self.save(owner);
                    }
                    Err(e) if e.kind() == ErrorKind::NotFound => {
                        sleep(Duration::from_millis(40)).await
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await
        .map_err(|_| io::Error::new(ErrorKind::TimedOut, "virtiofsd socket not ready"))?
    }

    pub(super) fn check_share_endpoints(
        &self,
        share: &Share,
    ) -> io::Result<(FileIdentity, FileIdentity)> {
        Ok((
            endpoint(&self.directory.join(SOCKET), true, share.socket)?,
            endpoint(&self.directory.join(PID_FILE), false, share.pid_file)?,
        ))
    }

    pub(super) async fn cleanup_share(&self, dir: &Path, share: &Share) -> io::Result<()> {
        Self::unlink_socket(&dir.join(SOCKET), SOCKET, share.socket).await?;
        let pid_file = dir.join(PID_FILE);
        match endpoint(&pid_file, false, share.pid_file) {
            Ok(_) => fs::remove_file(pid_file)?,
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let export = dir.join(EXPORT);
        let retired = dir.join(".retired-podfs");
        if share.volume_stage.is_some() || share.export.is_none() {
            volume::cleanup_stage(dir, share.volume_stage)?;
        }

        match fs::symlink_metadata(&export) {
            Ok(meta) if meta.is_dir() && Some(FileIdentity::from_meta(&meta)) == share.export => {
                let pinned_export = pin_directory(&export, share.export)?;
                pin_directory(&pinned(&pinned_export).join("containers"), share.containers)?;
                if share.volumes.is_some() {
                    pin_directory(&pinned(&pinned_export).join("volumes"), share.volumes)?;
                }
                no_host_mounts(&export)?;
                // Mark successful ownership verification before recursive deletion.
                // If deletion is interrupted it may already have removed containers/.
                nix::fcntl::renameat2(
                    None,
                    &export,
                    None,
                    &retired,
                    nix::fcntl::RenameFlags::RENAME_NOREPLACE,
                )
                .map_err(io::Error::from)?;
                File::open(dir)?.sync_all()?;
            }
            // Startup can be interrupted between mkdir and the first identity save.
            // No service could have started then. Only remove empty staging directories.
            Ok(meta) if meta.is_dir() && share.export.is_none() => {
                match fs::remove_dir(export.join("containers")) {
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    result => result?,
                }
                match fs::remove_dir(export.join("volumes")) {
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    result => result?,
                }
                fs::remove_dir(export)?;
            }
            Ok(_) => return Err(invalid("refusing to remove replaced Pod export")),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        match pin_directory(&retired, share.export) {
            Ok(export) => {
                no_host_mounts(&pinned(&export))?;
                fs::remove_dir_all(retired)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub(super) fn bundles(&self, owner: &Owner) -> io::Result<File> {
        let share = owner
            .share
            .as_ref()
            .ok_or_else(|| invalid("Pod has no virtiofs export"))?;
        let (_dir, control_path) = self.open_socket_directory(owner)?;
        let export = pin_directory(&control_path.join(EXPORT), share.export)?;
        pin_directory(&pinned(&export).join("containers"), share.containers)
    }
}

impl PodVmStore {
    pub(crate) async fn supports_rootfs_mount(&self, id: &str) -> io::Result<bool> {
        let Some(lease) = self.lease(id, Operation::Status).await? else {
            return Ok(false);
        };
        Ok(lease
            .load(id)?
            .and_then(|o| o.share)
            .is_some_and(|share| share.volume_catalog_mount.is_some()))
    }

    /// Atomically publish a host-private staged directory on the same filesystem.
    /// Nothing opens guest-supplied paths inside a published bundle.
    pub async fn publish_bundle(&self, id: &str, container: &str, staged: &Path) -> io::Result<()> {
        self.publish_bundle_with_root(id, container, staged, None)
            .await
    }

    pub(crate) async fn publish_bundle_with_root(
        &self,
        id: &str,
        container: &str,
        staged: &Path,
        view: Option<File>,
    ) -> io::Result<()> {
        if !canonical_uuid(container) {
            return Err(invalid("expected canonical container UUID"));
        }
        let missing = || io::Error::new(ErrorKind::NotFound, "VM attempt not found");
        let lease = self
            .lease(id, Operation::Remove)
            .await?
            .ok_or_else(missing)?;
        let mut owner = lease.load(id)?.ok_or_else(missing)?;
        if owner.removing || owner.boot.is_none() {
            return Err(invalid("VM is not accepting bundles"));
        }
        let bundles = lease.bundles(&owner)?;
        if !fs::symlink_metadata(staged)?.is_dir() {
            return Err(invalid("staged bundle is not a directory"));
        }
        no_host_mounts(staged)?;
        let worker = if let Some(view) = &view {
            let worker = lease.volume_worker(&owner).await?.ok_or_else(missing)?;
            let receipt = RootfsMount {
                container: container.into(),
                bundle: FileIdentity::check(staged)?,
                placeholder: FileIdentity::check(&staged.join("rootfs"))?,
                root: FileIdentity::from_meta(&view.metadata()?),
                mount: mount_id(view)?,
            };
            let share = owner.share.as_mut().expect("VM export");
            if share
                .rootfs
                .iter()
                .any(|mount| mount.container == container)
            {
                return Err(invalid("VM container already has a rootfs mount receipt"));
            }
            share.rootfs.push(receipt);
            lease.save(&owner)?;
            Some(worker)
        } else {
            None
        };
        nix::fcntl::renameat2(
            None,
            staged,
            Some(bundles.as_raw_fd()),
            Path::new(container),
            nix::fcntl::RenameFlags::RENAME_NOREPLACE,
        )
        .map_err(io::Error::from)?;
        bundles.sync_all()?;
        if let Some((worker, view)) = worker.zip(view) {
            let share = owner.share.as_ref().expect("VM export");
            share
                .rootfs
                .last()
                .expect("saved receipt")
                .attach(&worker, share, &view)?;
        }
        Ok(())
    }

    pub async fn remove_bundle(&self, id: &str, container: &str) -> io::Result<()> {
        if !canonical_uuid(container) {
            return Err(invalid("expected canonical container UUID"));
        }
        let Some(lease) = self.lease(id, Operation::Remove).await? else {
            return Ok(());
        };
        let Some(mut owner) = lease.load(id)? else {
            return Ok(());
        };
        lease.remove_volumes(&mut owner, container).await?;
        let bundles = lease.bundles(&owner)?;
        // Check the whole pinned directory, so an entry replaced by the guest
        // cannot redirect deletion into a different pre-existing mount in this export.
        no_host_mounts(&pinned(&bundles))?;
        let path = pinned(&bundles).join(container);
        if let Some(share) = &owner.share {
            if let Some(mount) = share
                .rootfs
                .iter()
                .find(|mount| mount.container == container)
            {
                match fs::symlink_metadata(&path) {
                    Ok(meta) if meta.is_dir() && FileIdentity::from_meta(&meta) == mount.bundle => {
                    }
                    Ok(_) => return Err(invalid("refusing to remove a replaced VM bundle")),
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
                if let Some(worker) = lease.volume_worker(&owner).await? {
                    mount.detach(&worker, share)?;
                }
                // Keep the receipt until the published bundle is gone.
            }
        }
        match fs::remove_dir_all(&path) {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
            Ok(()) => bundles.sync_all(),
        }?;
        let share = owner.share.as_mut().expect("VM export");
        let before = share.rootfs.len();
        share.rootfs.retain(|mount| mount.container != container);
        if before != share.rootfs.len() {
            lease.save(&owner)?;
        }
        Ok(())
    }
}
