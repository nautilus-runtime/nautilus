//! Cloud Hypervisor v55 locks api.sock.lock until its HTTP listener drops.
//! SIGKILL leaves the name behind. The existing launch token gives us an
//! inode and an attempt nonce to journal before publishing that name.
use super::*;
use std::os::unix::fs::FileExt;

const NAME: &str = "api.sock.lock";
const RECOVERY: &str = ".api-lock-recovery";

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(super) enum Receipt {
    Created(FileIdentity),
    Adopted(FileIdentity),
    Retained(FileIdentity),
}

fn lock_exclusive(file: &File) -> io::Result<()> {
    let mut range: libc::flock = unsafe { std::mem::zeroed() };
    range.l_type = libc::F_WRLCK as _;
    range.l_whence = libc::SEEK_SET as _;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &range) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Lease {
    pub(super) fn publish_api_lock(&self, owner: &mut Owner) -> io::Result<()> {
        let token = self.launch_token(owner);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&token)?;
        file.write_all(owner.nonce.as_bytes())?;
        file.sync_all()?;
        owner.api_lock = Some(Receipt::Created(FileIdentity::from_meta(&file.metadata()?)));
        self.save(owner)?;
        fs::hard_link(token, self.directory.join(NAME))?;
        File::open(&self.directory)?.sync_all()
    }

    // On upgrade a live invocation can prove the old inode belongs to it:
    // the verified MainPID must hold the corresponding whole-file OFD lock.
    pub(super) async fn adopt_live_api_lock(&self, owner: &mut Owner) -> io::Result<()> {
        if owner.api_lock.is_some() || owner.invocation.is_none() {
            return Ok(());
        }
        let Some(unit) = self.unit(owner, Service::Vmm).await? else {
            return Ok(());
        };
        let Some(pid) = unit.pid else { return Ok(()) };
        let (_dir, path) = self.open_socket_directory(owner)?;
        let meta = match fs::symlink_metadata(path.join(NAME)) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            other => other?,
        };
        let identity = FileIdentity::from_meta(&meta);
        if !meta.is_file() || meta.nlink() != 1 || meta.len() != 0 {
            return Ok(());
        }
        let process = PathBuf::from(format!("/proc/{pid}"));
        let files = match fs::read_dir(process.join("fd")) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            other => other?,
        };
        for entry in files {
            let entry = entry?;
            if !fs::metadata(entry.path()).is_ok_and(|m| FileIdentity::from_meta(&m) == identity) {
                continue;
            }
            let info = fs::read_to_string(process.join("fdinfo").join(entry.file_name()))
                .unwrap_or_default();
            if info
                .lines()
                .filter(|line| line.starts_with("lock:"))
                .any(|line| {
                    let fields: Vec<_> = line.split_whitespace().collect();
                    fields.len() == 9
                        && fields[2..5] == ["OFDLCK", "ADVISORY", "WRITE"]
                        && fields[6].rsplit(':').next().and_then(|i| i.parse().ok())
                            == Some(identity.inode)
                        && fields[7..9] == ["0", "EOF"]
                })
                && self
                    .unit(owner, Service::Vmm)
                    .await?
                    .is_some_and(|u| u.pid == Some(pid) && u.invocation == unit.invocation)
            {
                owner.api_lock = Some(Receipt::Adopted(identity));
                self.save(owner)?;
                break;
            }
        }
        Ok(())
    }

    fn recovery_file(&self, owner: &Owner, identity: FileIdentity) -> io::Result<(File, PathBuf)> {
        let path = self.root.join(RECOVERY);
        match DirBuilder::new().mode(0o700).create(&path) {
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            result => result?,
        }
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(&path)?;
        FileIdentity::directory(&path, &dir.metadata()?)?;
        let name = PathBuf::from(format!(
            "{}-{}-{}-{}-{NAME}",
            owner.id, owner.nonce, identity.device, identity.inode
        ));
        Ok((dir, name))
    }

    pub(super) fn release_api_lock(&self, owner: &mut Owner) -> io::Result<()> {
        let (dir, path) = match self.open_socket_directory(owner) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            other => other?,
        };
        let path = path.join(NAME);
        let meta = match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                if let Some(Receipt::Retained(id)) = owner.api_lock {
                    let (recovery, name) = self.recovery_file(owner, id)?;
                    let moved = fs::symlink_metadata(
                        PathBuf::from(format!("/proc/self/fd/{}", recovery.as_raw_fd())).join(name),
                    )?;
                    if FileIdentity::from_meta(&moved) != id {
                        return Err(invalid("retained API lock changed"));
                    }
                    recovery.sync_all()?;
                    dir.sync_all()?;
                }
                return Ok(());
            }
            other => other?,
        };
        if !meta.is_file()
            || meta.nlink() != 1
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o077 != 0
        {
            return Err(invalid(
                "unexpected VM API lock type, owner, permissions or links",
            ));
        }
        let id = FileIdentity::from_meta(&meta);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        lock_exclusive(&file)?;
        if FileIdentity::from_meta(&file.metadata()?) != id
            || FileIdentity::from_meta(&fs::symlink_metadata(&path)?) != id
            || volume::mount_id(&file)? != volume::mount_id(&dir)?
        {
            return Err(invalid("API lock identity or mount changed"));
        }
        match owner.api_lock {
            Some(Receipt::Created(expected)) if id == expected => {
                let mut bytes = [0; 36];
                file.read_exact_at(&mut bytes, 0)?;
                if meta.len() != 36 || bytes != owner.nonce.as_bytes() {
                    return Err(invalid("API lock nonce changed"));
                }
                fs::remove_file(path)?;
            }
            Some(Receipt::Adopted(expected)) if id == expected && meta.len() == 0 => {
                fs::remove_file(path)?
            }
            None | Some(Receipt::Retained(_)) => {
                if meta.len() != 0
                    || matches!(owner.api_lock, Some(Receipt::Retained(expected)) if id != expected)
                {
                    return Err(invalid("unverified legacy API lock"));
                }
                // Its old VMM is gone. Preserve this unproven inode under a
                // recoverable name rather than treating it as ours to delete.
                let (recovery, name) = self.recovery_file(owner, id)?;
                owner.api_lock = Some(Receipt::Retained(id));
                self.save(owner)?;
                nix::fcntl::renameat2(
                    Some(dir.as_raw_fd()),
                    Path::new(NAME),
                    Some(recovery.as_raw_fd()),
                    &name,
                    nix::fcntl::RenameFlags::RENAME_NOREPLACE,
                )?;
                recovery.sync_all()?;
                tracing::warn!(pod_id=%owner.id, path=%self.root.join(RECOVERY).join(name).display(), "Preserved unrecorded legacy VMM API lock");
            }
            _ => return Err(invalid("refusing to unlink replaced VM API lock")),
        }
        dir.sync_all()
    }
}
