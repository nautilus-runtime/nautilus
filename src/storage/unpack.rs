//! Apply OCI layers relative to an open rootfs directory.

use crate::core::{Error, Result};
use flate2::bufread::GzDecoder;
use pathrs::{error::ErrorKind, flags::OpenFlags, InodeType, Root};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{File, Permissions};
use std::io::{BufRead, Cursor, Read, Seek};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, PermissionsExt},
};
use std::path::{Component, Path, PathBuf};
use tar::Entry;

pub fn apply_layer(rootfs: &Path, blob: &[u8], media_type: &str) -> Result<()> {
    apply_layer_from(rootfs, Cursor::new(blob), media_type)
}

pub(crate) fn apply_layer_from(
    rootfs: &Path,
    mut blob: impl BufRead + Seek,
    media_type: &str,
) -> Result<()> {
    let root = Root::open(rootfs)?;
    // Whiteouts apply only to lower layers, regardless of their tar entry order.
    for entry in archive(&mut blob, media_type)?.entries()? {
        let entry = entry?;
        let name = sanitize_path(&entry.path()?)?;
        if let Some(target) = whiteout(&name)? {
            let parent = name.parent().unwrap_or(Path::new(""));
            match target {
                Whiteout::Child(child) => remove_if_present(&root, &parent.join(child))?,
                Whiteout::Opaque => clear_dir(&root, parent)?,
            }
        }
    }
    blob.rewind()?;
    let mut directories = BTreeMap::new();
    for entry in archive(&mut blob, media_type)?.entries()? {
        let entry = entry?;
        let name = sanitize_path(&entry.path()?)?;
        if whiteout(&name)?.is_some() {
            continue;
        }
        if entry.header().entry_type() == tar::EntryType::Directory {
            let name = if name.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                name
            };
            if name != Path::new(".") {
                if let Ok(handle) = root.resolve_nofollow(&name) {
                    if !File::from(OwnedFd::from(handle)).metadata()?.is_dir() {
                        remove_if_present(&root, &name)?;
                    }
                }
                root.mkdir_all(&name, &Permissions::from_mode(0o755))?;
            }
            directories.insert(name, Attributes::from_entry(&entry)?);
        } else if !name.as_os_str().is_empty() {
            directories.remove(&name);
            apply_entry(&root, &name, entry)?;
        }
    }
    // Apply children before parents: a private directory's final mode or
    // ownership must not prevent extraction of its children.
    for (name, attributes) in directories.into_iter().rev() {
        let dir = root.open_subpath(
            name,
            OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY | OpenFlags::O_NOFOLLOW,
        )?;
        attributes.apply(&dir)?;
    }
    Ok(())
}

fn archive<'a>(
    blob: &'a mut impl BufRead,
    media_type: &str,
) -> Result<tar::Archive<Box<dyn Read + 'a>>> {
    let reader: Box<dyn Read> = if media_type.contains("+gzip") || media_type.ends_with("tar.gzip")
    {
        Box::new(GzDecoder::new(blob))
    } else if media_type.contains("+zstd") || media_type.contains("tar.zstd") {
        Box::new(zstd::Decoder::with_buffer(blob)?)
    } else if media_type.contains("tar") {
        Box::new(blob)
    } else {
        return Err(Error::Image(format!(
            "unsupported layer media type: {media_type}"
        )));
    };
    Ok(tar::Archive::new(reader))
}

enum Whiteout<'a> {
    Child(&'a OsStr),
    Opaque,
}

fn whiteout(name: &Path) -> Result<Option<Whiteout<'_>>> {
    let Some(target) = name
        .file_name()
        .and_then(|s| s.as_bytes().strip_prefix(b".wh."))
    else {
        return Ok(None);
    };
    match target {
        b".wh..opq" => Ok(Some(Whiteout::Opaque)),
        b"" | b"." | b".." => Err(Error::Image(format!(
            "invalid whiteout: {}",
            name.display()
        ))),
        target => Ok(Some(Whiteout::Child(OsStr::from_bytes(target)))),
    }
}

fn apply_entry<R: Read>(root: &Root, name: &Path, mut entry: Entry<'_, R>) -> Result<()> {
    if let Some(parent) = name.parent().filter(|p| !p.as_os_str().is_empty()) {
        root.mkdir_all(parent, &Permissions::from_mode(0o755))?;
    }
    match entry.header().entry_type() {
        ty @ (tar::EntryType::Symlink | tar::EntryType::Link) => {
            let target = entry.link_name()?.ok_or_else(|| {
                Error::Image(format!("missing link target for {}", name.display()))
            })?;
            let inode = if ty == tar::EntryType::Symlink {
                InodeType::Symlink(target.into_owned())
            } else {
                InodeType::Hardlink(sanitize_path(&target)?)
            };
            remove_if_present(root, name)?;
            root.create(name, &inode)?;
            if ty == tar::EntryType::Symlink {
                let fd = File::from(OwnedFd::from(root.resolve_nofollow(name)?));
                Attributes::from_entry(&entry)?.set_owner(&fd, true)?;
            }
        }
        tar::EntryType::Regular => {
            let attributes = Attributes::from_entry(&entry)?;
            remove_if_present(root, name)?;
            let mut out = root.create_file(
                name,
                OpenFlags::O_WRONLY | OpenFlags::O_EXCL,
                &Permissions::from_mode(0o600),
            )?;
            std::io::copy(&mut entry, &mut out)?;
            attributes.apply(&out)?;
        }
        _ => {}
    }
    Ok(())
}

struct Attributes {
    mode: u32,
    uid: u32,
    gid: u32,
}

impl Attributes {
    fn from_entry<R: Read>(entry: &Entry<'_, R>) -> Result<Self> {
        let id = |raw: u64| {
            u32::try_from(raw)
                .ok()
                .filter(|v| *v != u32::MAX)
                .ok_or_else(|| Error::Image(format!("unrepresentable layer uid/gid: {raw}")))
        };
        Ok(Self {
            mode: entry.header().mode()? & 0o7777,
            uid: id(entry.header().uid()?)?,
            gid: id(entry.header().gid()?)?,
        })
    }

    fn set_owner(&self, file: &File, symlink: bool) -> Result<()> {
        let metadata = file.metadata()?;
        if (metadata.uid(), metadata.gid()) == (self.uid, self.gid) {
            return Ok(());
        }
        let result = if symlink {
            // O_PATH pins the link itself. AT_EMPTY_PATH avoids a second lookup.
            let rc = unsafe {
                libc::fchownat(
                    file.as_raw_fd(),
                    c"".as_ptr(),
                    self.uid,
                    self.gid,
                    libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        } else {
            std::os::unix::fs::fchown(file, Some(self.uid), Some(self.gid))
        };
        result.map_err(|err| {
            Error::Image(format!(
                "cannot set layer ownership {}:{}: {err}",
                self.uid, self.gid
            ))
        })
    }

    fn apply(&self, file: &File) -> Result<()> {
        self.set_owner(file, false)?;
        // chown can clear set-ID bits, so the mode must be applied last.
        file.set_permissions(Permissions::from_mode(self.mode))?;
        Ok(())
    }
}

fn sanitize_path(path: &Path) -> Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => result.push(name),
            Component::CurDir => {}
            _ => {
                return Err(Error::Image(format!(
                    "unsafe path in layer: {}",
                    path.display()
                )))
            }
        }
    }
    Ok(result)
}

fn remove_if_present(root: &Root, name: &Path) -> Result<()> {
    match root.remove_all(name) {
        Err(err) if err.kind() == ErrorKind::OsError(Some(libc::ENOENT)) => Ok(()),
        result => Ok(result?),
    }
}

fn clear_dir(root: &Root, name: &Path) -> Result<()> {
    let name = if name.as_os_str().is_empty() {
        Path::new(".")
    } else {
        name
    };
    let handle = match root.open_subpath(name, OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY) {
        Err(err) if err.kind() == ErrorKind::OsError(Some(libc::ENOENT)) => return Ok(()),
        result => result?,
    };
    // Only the trusted, pinned directory fd is named in procfs. The entry
    // names below are removed relative to that fd, never to a host pathname.
    let entries = std::fs::read_dir(format!("/proc/self/fd/{}", handle.as_fd().as_raw_fd()))?;
    let dir = Root::from_fd(handle);
    for entry in entries {
        remove_if_present(&dir, Path::new(&entry?.file_name()))?;
    }
    Ok(())
}

/// Replace a generated configuration file without following the image's leaf
/// symlink or modifying another name for the same inode through a hardlink.
pub(crate) fn write_etc_file(root: &Root, name: &str, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    root.mkdir_all("etc", &Permissions::from_mode(0o755))?;
    match root.remove_file(name) {
        Err(err) if err.kind() == ErrorKind::OsError(Some(libc::ENOENT)) => {}
        result => result?,
    }
    let mut out = root.create_file(
        name,
        OpenFlags::O_WRONLY | OpenFlags::O_EXCL,
        &Permissions::from_mode(0o644),
    )?;
    out.write_all(contents)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn layer_links_and_whiteouts_use_image_paths() {
        use std::io::{BufReader, Write};

        let mut tar = tar::Builder::new(Vec::new());
        let mut append = |path: &str, kind: tar::EntryType, target: Option<&str>, bytes: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_uid(unsafe { libc::geteuid() } as u64);
            header.set_gid(unsafe { libc::getegid() } as u64);
            if let Some(target) = target {
                header.set_link_name(target).unwrap();
            }
            tar.append_data(&mut header, path, bytes).unwrap();
        };
        append("view", tar::EntryType::Symlink, Some("/data"), b"");
        append("view/current", tar::EntryType::Regular, None, b"current");
        append("same", tar::EntryType::Link, Some("view/current"), b"");
        append("data/.wh..wh..opq", tar::EntryType::Regular, None, b"");
        append(".wh.obsolete", tar::EntryType::Regular, None, b"");
        let tar = tar.into_inner().unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&tar).unwrap();
        let zstd = zstd::stream::encode_all(tar.as_slice(), 1).unwrap();

        for (suffix, bytes) in [
            ("tar", tar),
            ("tar+gzip", gzip.finish().unwrap()),
            ("tar+zstd", zstd),
        ] {
            let root = tempfile::tempdir().unwrap();
            let data = root.path().join("data");
            std::fs::create_dir(&data).unwrap();
            std::fs::write(data.join("old"), b"previous").unwrap();
            std::fs::write(root.path().join("obsolete"), b"previous").unwrap();
            let mut archive = tempfile::NamedTempFile::new().unwrap();
            archive.write_all(&bytes).unwrap();
            apply_layer_from(
                root.path(),
                BufReader::new(archive.reopen().unwrap()),
                &format!("application/vnd.oci.image.layer.v1.{suffix}"),
            )
            .unwrap();
            assert!(!data.join("old").exists());
            assert!(!root.path().join("obsolete").exists());
            assert_eq!(std::fs::read(data.join("current")).unwrap(), b"current");
            assert_eq!(
                data.join("current").metadata().unwrap().ino(),
                root.path().join("same").metadata().unwrap().ino()
            );
        }
    }
    #[test]
    fn layer_retains_owners_and_directory_mode() {
        let root = tempfile::tempdir().unwrap();
        // Mapped root can exercise ownership changes. Ordinary users can
        // still unpack entries belonging to their current uid/gid.
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let (owner, group) = if uid == 0
            && std::fs::read_to_string("/proc/self/uid_map")
                .is_ok_and(|map| map.split_whitespace().eq(["0", "0", "4294967295"]))
        {
            (10001, 10002)
        } else {
            (uid, gid)
        };
        let mut tar = tar::Builder::new(Vec::new());
        for (name, ty, mode) in [
            ("app", tar::EntryType::Directory, 0o750),
            ("app/private", tar::EntryType::Regular, 0o600),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(ty);
            h.set_size(0);
            h.set_mode(mode);
            h.set_uid(u64::from(owner));
            h.set_gid(u64::from(group));
            tar.append_data(&mut h, name, std::io::empty()).unwrap();
        }
        apply_layer(
            root.path(),
            &tar.into_inner().unwrap(),
            "application/vnd.oci.image.layer.v1.tar",
        )
        .unwrap();
        for (name, mode) in [("app", 0o750), ("app/private", 0o600)] {
            let m = root.path().join(name).metadata().unwrap();
            assert_eq!((m.uid(), m.gid(), m.mode() & 0o7777), (owner, group, mode));
        }
    }
}
