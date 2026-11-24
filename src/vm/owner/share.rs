//! A private virtiofsd and writable OCI bundles for one Pod attempt.

use super::*;

const EXPORT: &str = "podfs";
const SOCKET: &str = "fs.sock";
const PID_FILE: &str = "fs.sock.pid";

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

fn pin_directory(path: &Path, expected: Option<FileIdentity>) -> io::Result<File> {
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    if Some(FileIdentity::from_meta(&dir.metadata()?)) != expected {
        return Err(invalid("Pod export directory identity changed"));
    }
    Ok(dir)
}

fn pinned(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}

impl Lease {
    pub(super) async fn start_share(&self, owner: &mut Owner, executable: &Path) -> io::Result<()> {
        let export = self.directory.join(EXPORT);
        let containers = export.join("containers");
        DirBuilder::new().mode(0o700).create(&export)?;
        DirBuilder::new().mode(0o700).create(&containers)?;
        let share = owner.share.as_mut().expect("share requested");
        share.export = Some(FileIdentity::check(&export)?);
        share.containers = Some(FileIdentity::check(&containers)?);
        File::open(&export)?.sync_all()?;
        File::open(&self.directory)?.sync_all()?;
        self.save(owner)?;
        let mut cmd = self.service_command(owner, Service::Virtiofsd);
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
        match fs::symlink_metadata(&export) {
            Ok(meta) if meta.is_dir() && Some(FileIdentity::from_meta(&meta)) == share.export => {
                fs::remove_dir_all(export)?;
            }
            // Startup can be interrupted between mkdir and the first identity save.
            // No service could have started then. Only remove empty staging directories.
            Ok(meta) if meta.is_dir() && share.export.is_none() => {
                match fs::remove_dir(export.join("containers")) {
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    result => result?,
                }
                fs::remove_dir(export)?;
            }
            Ok(_) => return Err(invalid("refusing to remove replaced Pod export")),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(())
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
    /// Atomically publish a host-private staged directory on the same filesystem.
    /// Nothing opens guest-supplied paths inside a published bundle.
    pub async fn publish_bundle(&self, id: &str, container: &str, staged: &Path) -> io::Result<()> {
        if !canonical_uuid(container) {
            return Err(invalid("expected canonical container UUID"));
        }
        let missing = || io::Error::new(ErrorKind::NotFound, "VM attempt not found");
        let lease = self
            .lease(id, Operation::Remove)
            .await?
            .ok_or_else(missing)?;
        let owner = lease.load(id)?.ok_or_else(missing)?;
        if owner.removing || owner.boot.is_none() {
            return Err(invalid("VM is not accepting bundles"));
        }
        let bundles = lease.bundles(&owner)?;
        if !fs::symlink_metadata(staged)?.is_dir() {
            return Err(invalid("staged bundle is not a directory"));
        }
        nix::fcntl::renameat2(
            None,
            staged,
            Some(bundles.as_raw_fd()),
            Path::new(container),
            nix::fcntl::RenameFlags::RENAME_NOREPLACE,
        )
        .map_err(io::Error::from)?;
        bundles.sync_all()
    }

    pub async fn remove_bundle(&self, id: &str, container: &str) -> io::Result<()> {
        if !canonical_uuid(container) {
            return Err(invalid("expected canonical container UUID"));
        }
        let Some(lease) = self.lease(id, Operation::Remove).await? else {
            return Ok(());
        };
        let Some(owner) = lease.load(id)? else {
            return Ok(());
        };
        let bundles = lease.bundles(&owner)?;
        let path = pinned(&bundles).join(container);
        match fs::remove_dir_all(&path) {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
            Ok(()) => bundles.sync_all(),
        }
    }
}
