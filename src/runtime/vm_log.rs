//! Copy CRI records from the authenticated guest log into kubelet's host file.

use serde::{Deserialize, Serialize};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::future::Future;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, DirEntryExt, FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

const CHUNK: i64 = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
}

impl Identity {
    fn of(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
}

/// Persist with ContainerState, after fsync and before issuing another read.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Cursor {
    parent: Identity,
    file: Identity,
    offset: i64,
    // Guest offset corresponding to byte zero of the current host file.
    base: i64,
    // Last committed bytes detect truncation followed by unrelated writes.
    tail: Vec<u8>,
}

#[derive(Debug)]
pub struct Target {
    path: PathBuf,
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn at(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}

impl Target {
    pub fn new(log_dir: &Path, relative: &Path, storage: &Path, id: &str) -> io::Result<Self> {
        if !uuid::Uuid::parse_str(id).is_ok_and(|v| !v.is_nil() && v.to_string() == id) {
            return Err(invalid("invalid VM container ID"));
        }
        if !relative.as_os_str().is_empty()
            && (relative.as_os_str().as_bytes().ends_with(b"/")
                || relative.file_name().is_none()
                || !relative
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))))
        {
            return Err(invalid(
                "VM log path must be relative to the sandbox log directory",
            ));
        }
        let path = if log_dir.as_os_str().is_empty() || relative.as_os_str().is_empty() {
            storage.join(format!("{id}-out")).join("out.log")
        } else {
            log_dir.join(relative)
        };
        if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(invalid(
                "VM log directory must be an absolute path without '..'",
            ));
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    // Walk one component at a time. Keep its parent open across mkdir/open so
    // renaming a path or substituting a symlink cannot redirect a host write.
    fn parent(&self, create: bool) -> io::Result<File> {
        let mut parent = File::open("/")?;
        for component in self.path.parent().unwrap().components() {
            let Component::Normal(name) = component else {
                continue;
            };
            let next = at(&parent).join(name);
            let open = || {
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
                    .open(&next)
            };
            parent = match open() {
                Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                    match DirBuilder::new().mode(0o700).create(&next) {
                        Ok(()) => parent.sync_all()?,
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e),
                    }
                    open()?
                }
                result => result?,
            };
        }
        Ok(parent)
    }

    fn open(&self, parent: &File, create_new: bool) -> io::Result<File> {
        Self::open_private(&at(parent).join(self.path.file_name().unwrap()), create_new)
    }

    fn open_private(path: &Path, create_new: bool) -> io::Result<File> {
        let f = if create_new {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?
        } else {
            // Inspect the inode without activating a FIFO or device. The procfd
            // re-open below refers to the verified inode even if the name moves.
            let pin = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
                .open(path)?;
            Self::check_private(&pin)?;
            OpenOptions::new().read(true).write(true).open(at(&pin))?
        };
        Self::check_private(&f)?;
        Ok(f)
    }

    fn check_private(f: &File) -> io::Result<()> {
        let m = f.metadata()?;
        if !m.is_file()
            || m.nlink() != 1
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o077 != 0
        {
            return Err(invalid(
                "VM log must be a private regular file with a single link",
            ));
        }
        Ok(())
    }

    fn rotated(dir: &File, saved: Identity) -> io::Result<Option<File>> {
        for entry in fs::read_dir(at(dir))? {
            let entry = entry?;
            if entry.ino() == saved.ino {
                match Self::open_private(&entry.path(), false) {
                    Ok(f) if Identity::of(&f)? == saved => return Ok(Some(f)),
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(None)
    }

    pub fn prepare(&self) -> io::Result<Cursor> {
        let dir = self.parent(true)?;
        let file = self.open(&dir, true)?;
        file.sync_all()?;
        dir.sync_all()?;
        Ok(Cursor {
            parent: Identity::of(&dir)?,
            file: Identity::of(&file)?,
            ..Cursor::default()
        })
    }

    /// Release an empty file claimed by an unsuccessful CreateContainer. A
    /// successful container's log belongs to kubelet after container removal.
    pub fn discard_unstarted(&self, cursor: &Cursor) -> io::Result<()> {
        if cursor.offset != 0 || cursor.base != 0 || !cursor.tail.is_empty() {
            return Err(invalid("refusing to discard a VM log with committed data"));
        }
        let dir = match self.parent(false) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            result => result?,
        };
        if Identity::of(&dir)? != cursor.parent {
            return Err(invalid("VM log parent identity changed"));
        }
        let file = match self.open(&dir, false) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            result => result?,
        };
        let path = at(&dir).join(self.path.file_name().unwrap());
        let meta = fs::symlink_metadata(&path)?;
        if Identity::of(&file)? != cursor.file
            || !meta.is_file()
            || meta.nlink() != 1
            || (meta.dev(), meta.ino()) != (cursor.file.dev, cursor.file.ino)
            || meta.len() != 0
        {
            return Err(invalid("refusing to discard changed VM log"));
        }
        fs::remove_file(path)?;
        dir.sync_all()
    }

    /// Kubelet explicitly requested rotation. Bind a fresh empty host file at
    /// the last committed guest offset. Persist this result before copying.
    pub fn reopen(&self, cursor: &Cursor) -> io::Result<Cursor> {
        let dir = self.parent(false)?;
        if Identity::of(&dir)? != cursor.parent {
            return Err(invalid("VM log parent identity changed"));
        }
        let current = match self.open(&dir, false) {
            Ok(f) if Identity::of(&f)? == cursor.file => {
                cursor.verify(&f)?;
                return Ok(cursor.clone());
            }
            Ok(f) if f.metadata()?.len() == 0 => Some(f),
            Ok(_) => return Err(invalid("replacement VM log already contains data")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if let Some(previous) = Self::rotated(&dir, cursor.file)? {
            if previous.metadata()?.len() != cursor.verify(&previous)? {
                return Err(invalid("replay the uncommitted guest log before rotating"));
            }
        }
        let file = match current {
            Some(file) => file,
            None => self.open(&dir, true)?,
        };
        let identity = Identity::of(&file)?;
        file.sync_all()?;
        dir.sync_all()?;
        Ok(Cursor {
            parent: cursor.parent,
            file: identity,
            base: cursor.offset,
            offset: cursor.offset,
            tail: Vec::new(),
        })
    }

    /// Read at most one chunk via the caller's authenticated guest connection.
    /// A full chunk may have a successor. A short read reaches the current end.
    pub async fn sync_chunk<F>(
        &self,
        cursor: &Cursor,
        read: impl FnOnce(i64, i64) -> F,
    ) -> crate::core::Result<(Cursor, bool)>
    where
        F: Future<Output = crate::core::Result<(Vec<u8>, i64)>>,
    {
        let dir = self.parent(false)?;
        if Identity::of(&dir)? != cursor.parent {
            return Err(invalid("VM log parent identity changed").into());
        }
        let file = match self.open(&dir, false) {
            Ok(f) if Identity::of(&f)? == cursor.file => Some(f),
            Ok(_) => Self::rotated(&dir, cursor.file)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::rotated(&dir, cursor.file)?,
            Err(e) => return Err(e.into()),
        }
        .ok_or_else(|| invalid("VM log target changed; reopen required"))?;
        let position = cursor.verify(&file)?;
        let (bytes, next) = read(cursor.offset, CHUNK).await?;
        if bytes.len() > CHUNK as usize
            || cursor.offset.checked_add(bytes.len() as i64) != Some(next)
        {
            return Err(invalid("discontinuous guest log response").into());
        }
        let len = file.metadata()?.len();
        let ahead = len
            .checked_sub(position)
            .filter(|n| *n <= bytes.len() as u64)
            .ok_or_else(|| invalid("host log length disagrees with guest cursor"))?
            as usize;
        // The previous copy may have been interrupted after a partial write or
        // fsync but before the sled cursor commit. Verify it before resuming.
        if ahead > 0 {
            let mut existing = vec![0; ahead];
            file.read_exact_at(&mut existing, position)?;
            if existing != bytes[..ahead] {
                return Err(invalid("uncommitted host log differs from guest log").into());
            }
        }
        if bytes.is_empty() {
            return Ok((cursor.clone(), false));
        }
        file.write_all_at(&bytes[ahead..], len)?;
        file.sync_data()?;
        if file.metadata()?.nlink() != 1 {
            return Err(invalid("VM log link changed while syncing").into());
        }
        let mut updated = cursor.clone();
        updated.offset = next;
        updated.tail = bytes[bytes.len().saturating_sub(32)..].to_vec();
        Ok((updated, bytes.len() == CHUNK as usize))
    }
}

impl Cursor {
    fn verify(&self, file: &File) -> io::Result<u64> {
        if self.base < 0 || self.offset < self.base {
            return Err(invalid("invalid VM log cursor"));
        }
        let at = (self.offset - self.base) as u64;
        if self.tail.len() > 32 || self.tail.len() as u64 > at || file.metadata()?.len() < at {
            return Err(invalid("host log truncated or cursor is invalid"));
        }
        let mut tail = vec![0; self.tail.len()];
        file.read_exact_at(&mut tail, at - self.tail.len() as u64)?;
        if tail != self.tail {
            return Err(invalid("committed host log boundary changed"));
        }
        Ok(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use uuid::Uuid;

    fn target(root: &Path) -> Target {
        Target::new(
            root,
            Path::new("app/0.log"),
            root,
            &Uuid::new_v4().to_string(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_crash_after_fsync_replays_bytes_once_and_rotation_preserves_guest_offset() {
        let work = tempfile::tempdir().unwrap();
        let target = target(work.path());
        let original = target.prepare().unwrap();
        let first = |offset, limit| async move {
            assert_eq!(offset, 0);
            assert_eq!(limit, 65536);
            Ok((b"first\n".to_vec(), 6))
        };
        let (cursor, more) = target.sync_chunk(&original, first).await.unwrap();
        assert!(!more);
        let _ = target.sync_chunk(&original, first).await.unwrap();
        assert_eq!(std::fs::read(target.path()).unwrap(), b"first\n");
        let rotated = target.path().with_extension("old");
        std::fs::rename(target.path(), &rotated).unwrap();
        let reopened = target.reopen(&cursor).unwrap();
        target
            .sync_chunk(&reopened, |offset, _| async move {
                assert_eq!(offset, 6);
                Ok((b"next\n".to_vec(), 11))
            })
            .await
            .unwrap();
        assert_eq!(std::fs::read(target.path()).unwrap(), b"next\n");
        assert_eq!(std::fs::read(rotated).unwrap(), b"first\n");
    }

    #[test]
    fn target_rejects_traversal_symlinks_and_two_containers_claiming_the_same_file() {
        let work = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4().to_string();
        for invalid in ["/outside", "../outside", "a/../../outside"] {
            assert!(Target::new(work.path(), Path::new(invalid), work.path(), &id).is_err());
        }
        let t = target(work.path());
        let external = tempfile::tempdir().unwrap();
        symlink(external.path(), work.path().join("app")).unwrap();
        assert!(t.prepare().is_err());
        assert!(external.path().read_dir().unwrap().next().is_none());
        std::fs::remove_file(work.path().join("app")).unwrap();
        t.prepare().unwrap();
        assert!(target(work.path()).prepare().is_err());
    }
    #[tokio::test]
    async fn partial_writes_resume_and_log_discontinuities_fail_closed() {
        let work = tempfile::tempdir().unwrap();
        let t = target(work.path());
        let initial = t.prepare().unwrap();
        std::fs::write(t.path(), b"abc").unwrap(); // Interrupted write, cursor not committed.
        let read = |_, _| async { Ok((b"abcdef".to_vec(), 6)) };
        let (cursor, _) = t.sync_chunk(&initial, read).await.unwrap();
        assert_eq!(std::fs::read(t.path()).unwrap(), b"abcdef");
        assert!(t
            .sync_chunk(&cursor, |_, _| async { Ok((vec![1], 9)) })
            .await
            .is_err());
        std::fs::write(t.path(), b"xyzdef").unwrap();
        assert!(t
            .sync_chunk(&cursor, |_, _| async { Ok((vec![], 6)) })
            .await
            .is_err());
        std::fs::write(t.path(), b"prefix").unwrap();
        assert!(t.sync_chunk(&initial, read).await.is_err());
        std::fs::hard_link(t.path(), work.path().join("extra-link")).unwrap();
        assert!(t.reopen(&initial).is_err());
    }
    #[tokio::test]
    async fn rotation_after_an_uncommitted_write_requires_replaying_the_old_inode() {
        let work = tempfile::tempdir().unwrap();
        let target = target(work.path());
        let initial = target.prepare().unwrap();
        let read = |offset, _| async move {
            assert_eq!(offset, 0);
            Ok((b"line\n".to_vec(), 5))
        };
        target.sync_chunk(&initial, read).await.unwrap(); // crash before cursor persistence.
        let rotated = target.path().with_extension("old");
        std::fs::rename(target.path(), &rotated).unwrap();
        assert!(target.reopen(&initial).is_err());
        let (recovered, _) = target.sync_chunk(&initial, read).await.unwrap();
        let active = target.reopen(&recovered).unwrap();
        target
            .sync_chunk(&active, |offset, _| async move {
                assert_eq!(offset, 5);
                Ok((Vec::new(), 5))
            })
            .await
            .unwrap();
        assert_eq!(std::fs::read(rotated).unwrap(), b"line\n");
        assert!(std::fs::read(target.path()).unwrap().is_empty());
    }
    #[test]
    fn failed_create_releases_only_the_empty_file_it_claimed() {
        let work = tempfile::tempdir().unwrap();
        let t = target(work.path());
        let claimed = t.prepare().unwrap();
        let backup = t.path().with_extension("old");
        std::fs::rename(t.path(), &backup).unwrap();
        let replacement = t.prepare().unwrap();
        assert!(t.discard_unstarted(&claimed).is_err());
        std::fs::write(t.path(), b"uncommitted").unwrap();
        assert!(t.discard_unstarted(&replacement).is_err());
        std::fs::remove_file(t.path()).unwrap();
        std::fs::rename(backup, t.path()).unwrap();
        t.discard_unstarted(&claimed).unwrap();
        t.discard_unstarted(&claimed).unwrap();
        assert!(!t.path().exists());
        t.prepare().unwrap();
    }
}
