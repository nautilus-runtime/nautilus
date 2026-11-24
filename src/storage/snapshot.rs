//! Persistent, immutable image trees and per-bundle OverlayFS views.
//! Cache and bundles share a filesystem: a hard link pins a tree even across
//! daemon restarts. Locks serialize publication, pinning and eviction per shard.
use crate::core::{Error, Result, StateManager};
use nix::errno::Errno;
use nix::mount::{mount, umount, MsFlags};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::Path;

const STORE: &str = ".snapshots-v1";
const PIN: &str = "snapshot.v1";
const OWNER: &str = ".nautilus-bundle-v1";

fn invalid(msg: &str) -> Error {
    Error::Storage(msg.into())
}

fn image_key(id: &str) -> String {
    format!("{:x}", Sha256::digest(id.as_bytes()))
}

pub(super) fn key(image_id: &str, manifest: &[u8]) -> String {
    format!("{}-{:x}", image_key(image_id), Sha256::digest(manifest))
}

fn valid_key(key: &str) -> bool {
    key.len() == 129
        && key.as_bytes()[64] == b'-'
        && key
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 64 || b.is_ascii_hexdigit())
}

fn lock(store: &Path, key: &str) -> Result<File> {
    use nix::fcntl::{flock, FlockArg};
    use std::os::fd::AsRawFd;
    if !valid_key(key) {
        return Err(invalid("invalid rootfs snapshot key"));
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(store)?;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(store.join(format!(".lock-{}", &key[..2])))?;
    flock(file.as_raw_fd(), FlockArg::LockExclusive).map_err(std::io::Error::from)?;
    Ok(file)
}

fn collect(tree: &Path) -> Result<()> {
    if tree.join("retired").try_exists()? && fs::metadata(tree.join("pin"))?.nlink() == 1 {
        let key = tree.file_name().unwrap().to_str().unwrap();
        let trash = tree.with_file_name(format!(".deleting-{key}"));
        if trash.try_exists()? {
            fs::remove_dir_all(&trash)?;
        }
        fs::rename(tree, &trash)?;
        fs::remove_dir_all(trash)?;
    }
    Ok(())
}

/// Claim a new bundle before preparing its rootfs. A stale or existing bundle
/// cannot be silently reused by another create.
pub(crate) fn begin(bundle: &Path) -> Result<()> {
    let parent = bundle
        .parent()
        .ok_or_else(|| invalid("invalid bundle path"))?;
    fs::create_dir_all(parent)?;
    fs::DirBuilder::new().mode(0o700).create(bundle)?;
    File::create(bundle.join(OWNER))?;
    Ok(())
}

fn mount_flags(options: &[String]) -> Result<MsFlags> {
    let mut flags = MsFlags::MS_NODEV;
    for option in options {
        flags |= match option.as_str() {
            "nodev" => MsFlags::MS_NODEV,
            "nosuid" => MsFlags::MS_NOSUID,
            "noexec" => MsFlags::MS_NOEXEC,
            "noatime" => MsFlags::MS_NOATIME,
            "relatime" => MsFlags::MS_RELATIME,
            _ => {
                return Err(Error::Config(format!(
                    "unsupported rootfs mount option: {option}"
                )))
            }
        };
    }
    Ok(flags)
}

pub(super) enum StagedTree {
    Existing,
    New(tempfile::TempDir),
    Plain,
}

fn overlay_path(path: &Path, flags: MsFlags) -> Result<bool> {
    if path.to_str().is_none()
        || path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(|b| b",:\\".contains(b))
    {
        if flags != MsFlags::MS_NODEV {
            return Err(invalid(
                "rootfs path cannot represent requested mount options",
            ));
        }
        return Ok(false);
    }
    Ok(true)
}

fn stage_tree(
    store: &Path,
    key: &str,
    flags: MsFlags,
    unpack: impl FnOnce(&Path) -> Result<()>,
) -> Result<StagedTree> {
    if store.join(key).try_exists()? {
        return Ok(StagedTree::Existing);
    }
    let staging = tempfile::Builder::new()
        .prefix(&format!(".building-{key}."))
        .tempdir_in(store)?;
    let root = staging.path().join("root");
    fs::create_dir(&root)?;
    // Probe before extraction so unsupported filesystems only unpack once, at create.
    let probe = tempfile::tempdir_in(staging.path())?;
    for name in ["upper", "work", "rootfs"] {
        fs::create_dir(probe.path().join(name))?;
    }
    let supported = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                private_namespace()?;
                mount_view(probe.path(), staging.path(), flags)?;
                umount(&probe.path().join("rootfs"))
            })
            .join()
    })
    .map_err(|_| invalid("snapshot mount probe panicked"))?;
    probe.close()?;
    match supported {
        Ok(()) => {}
        Err(Errno::ENODEV | Errno::EOPNOTSUPP | Errno::EPERM | Errno::EINVAL | Errno::EXDEV) => {
            if flags != MsFlags::MS_NODEV {
                return Err(invalid(
                    "filesystem cannot apply requested rootfs mount flags",
                ));
            }
            return Ok(StagedTree::Plain);
        }
        Err(e) => return Err(Error::Storage(format!("probe OverlayFS: {e}"))),
    }
    unpack(&root)?;
    fs::write(staging.path().join("pin"), key)?;
    Ok(StagedTree::New(staging))
}

pub(super) fn stage_pull(
    dir: &Path,
    key: &str,
    options: &[String],
    unpack: impl FnOnce(&Path) -> Result<()>,
) -> Result<StagedTree> {
    let store = dir.join(STORE);
    // Extraction uses a private staging directory. Recheck the hint under lock at publication.
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&store)?;
    let store = fs::canonicalize(store)?;
    let flags = mount_flags(options)?;
    if !overlay_path(&store, flags)? {
        return Ok(StagedTree::Plain);
    }
    stage_tree(&store, key, flags, unpack)
}

// Called with the shard locked. Another reference may already have published this tree.
fn install(tree: &Path, staged: StagedTree) -> Result<bool> {
    match staged {
        StagedTree::Plain => Ok(false),
        StagedTree::Existing => Ok(true),
        StagedTree::New(dir) => {
            fs::rename(dir.path(), tree)?;
            Ok(true)
        }
    }
}

pub(super) fn publish_pull(
    dir: &Path,
    key: &str,
    options: &[String],
    staged: Option<StagedTree>,
    unpack: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    if matches!(staged, Some(StagedTree::Plain)) {
        return Ok(());
    }
    let store = dir.join(STORE);
    let _lock = lock(&store, key)?;
    let store = fs::canonicalize(store)?;
    let tree = store.join(key);
    if !tree.try_exists()? {
        // Replay after interruption, or a cached tree evicted between staging and commit.
        let staged = match staged {
            Some(fresh @ StagedTree::New(_)) => fresh,
            _ => {
                let flags = mount_flags(options)?;
                if !overlay_path(&store, flags)? {
                    return Ok(());
                }
                stage_tree(&store, key, flags, unpack)?
            }
        };
        install(&tree, staged)?;
    }
    let retired = tree.join("retired");
    if retired.try_exists()? {
        fs::remove_file(retired)?;
    }
    Ok(())
}

/// Attach the shared tree, building on demand for an older store or a direct create.
pub(super) fn prepare(
    bundle: &Path,
    key: &str,
    options: &[String],
    unpack: impl Fn(&Path) -> Result<()>,
) -> Result<()> {
    let flags = mount_flags(options)?;
    let bundle = fs::canonicalize(bundle)?;
    if !overlay_path(&bundle, flags)? {
        return unpack(&bundle.join("rootfs"));
    }
    let store = bundle
        .parent()
        .ok_or_else(|| invalid("invalid bundle"))?
        .join(STORE);
    let _lock = lock(&store, key)?;
    let tree = store.join(key);
    if !install(&tree, stage_tree(&store, key, flags, &unpack)?)? {
        return unpack(&bundle.join("rootfs"));
    }
    fs::hard_link(tree.join("pin"), bundle.join(PIN))?;
    for name in ["upper", "work"] {
        fs::create_dir(bundle.join(name))?;
    }
    fs::write(
        bundle.join("overlay-options.json"),
        serde_json::to_vec(options)?,
    )?;
    Ok(())
}

fn mount_view(bundle: &Path, tree: &Path, flags: MsFlags) -> nix::Result<()> {
    let options = format!(
        "lowerdir={},upperdir={},workdir={},index=on",
        tree.join("root").display(),
        bundle.join("upper").display(),
        bundle.join("work").display()
    );
    mount(
        Some("overlay"),
        &bundle.join("rootfs"),
        Some("overlay"),
        flags,
        Some(options.as_str()),
    )
}

// Never change the namespace of a reusable Tokio worker. A dedicated thread
// owns this namespace; no pod holder or unrelated workload inherits its mount.
fn private_namespace() -> nix::Result<()> {
    use nix::sched::{unshare, CloneFlags};
    unshare(CloneFlags::CLONE_NEWNS)?;
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_SLAVE,
        None::<&str>,
    )
}

// A concurrent init can briefly retain a reference to the view across fork/exec.
fn retry_busy(mut operation: impl FnMut() -> nix::Result<()>) -> nix::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    loop {
        match operation() {
            Err(Errno::EBUSY) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            result => return result,
        }
    }
}

pub(crate) fn with_view<T: Send>(
    bundle: &Path,
    action: impl FnOnce() -> Result<T> + Send,
) -> Result<T> {
    if !bundle.join(PIN).try_exists()? {
        if bundle.join(OWNER).try_exists()? && bundle.join("upper").try_exists()? {
            return Err(invalid("writable snapshot has no image pin"));
        }
        return action();
    }
    let bundle = fs::canonicalize(bundle)?;
    let key = fs::read_to_string(bundle.join(PIN))?;
    if !valid_key(&key) {
        return Err(invalid("invalid bundle pin"));
    }
    let tree = bundle.parent().unwrap().join(STORE).join(key);
    // Serialize operations on this upperdir, without serializing siblings.
    let options = view_lock(&bundle)?.ok_or_else(|| invalid("bundle has no overlay options"))?;
    let flags = mount_flags(&serde_json::from_reader::<_, Vec<String>>(&options)?)?;
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                private_namespace().map_err(std::io::Error::from)?;
                // A just-stopped init may still be releasing its superblock.
                retry_busy(|| mount_view(&bundle, &tree, flags))
                    .map_err(|e| Error::Storage(format!("mount writable rootfs: {e}")))?;
                let result = action();
                // The running container keeps its own root mount. Other descendants
                // (e.g. an stdin holder) must not keep a second mount on this upper.
                let root = bundle.join("rootfs");
                retry_busy(|| umount(&root))
                    .map_err(|e| Error::Storage(format!("unmount writable rootfs: {e}")))?;
                result
            })
            .join()
    })
    .map_err(|_| invalid("rootfs view worker panicked"))?
}

// Refuse recursive deletion if an unexpected host mount is inside the bundle.
fn check_unmounted(bundle: &Path) -> Result<()> {
    let base = bundle
        .to_string_lossy()
        .replace('\\', "\\134")
        .replace(' ', "\\040")
        .replace('\t', "\\011")
        .replace('\n', "\\012");
    let prefix = format!("{base}/");
    let mounts = fs::read("/proc/thread-self/mountinfo")?;
    if String::from_utf8_lossy(&mounts)
        .lines()
        .filter_map(|l| l.split_whitespace().nth(4))
        .any(|p| p == base || p.starts_with(&prefix))
    {
        return Err(invalid("unexpected mount in container bundle"));
    }
    Ok(())
}

pub(crate) fn view_lock(bundle: &Path) -> Result<Option<File>> {
    let options = match File::open(bundle.join("overlay-options.json")) {
        Ok(file) => {
            use std::os::fd::AsRawFd;
            nix::fcntl::flock(file.as_raw_fd(), nix::fcntl::FlockArg::LockExclusive)
                .map_err(std::io::Error::from)?;
            Some(file)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    Ok(options)
}

pub(crate) fn remove(bundle: &Path) -> Result<()> {
    let _guard = view_lock(bundle)?;
    remove_locked(bundle)
}

pub(crate) fn remove_locked(bundle: &Path) -> Result<()> {
    if !bundle.try_exists()? {
        return Ok(());
    }
    if !fs::symlink_metadata(bundle)?.is_dir() {
        return Err(invalid("container bundle must be a directory"));
    }
    let bundle = fs::canonicalize(bundle)?;
    check_unmounted(&bundle)?;
    if !bundle.join(PIN).try_exists()? {
        fs::remove_dir_all(bundle)?;
        return Ok(());
    }
    let key = fs::read_to_string(bundle.join(PIN))?;
    let store = bundle.parent().unwrap().join(STORE);
    let _lock = lock(&store, &key)?;
    fs::remove_dir_all(&bundle)?;
    collect(&store.join(&key))
}

/// An explicit pull can reacquire an image removed while a bundle pinned it.
pub(super) fn retain(container_dir: &Path, id: &str, digest: &str) -> Result<()> {
    let store = container_dir.join(STORE);
    let key = key(id, digest.as_bytes());
    let marker = store.join(&key).join("retired");
    if marker.try_exists()? {
        let _lock = lock(&store, &key)?;
        if marker.try_exists()? {
            fs::remove_file(marker)?;
        }
    }
    Ok(())
}

pub(crate) fn retire(container_dir: &Path, image_id: &str) -> Result<()> {
    let store = container_dir.join(STORE);
    if !store.try_exists()? || image_id.is_empty() {
        return Ok(());
    }
    let prefix = format!("{}-", image_key(image_id));
    for entry in fs::read_dir(&store)? {
        let entry = entry?;
        let name = entry.file_name();
        let key = name.to_string_lossy();
        if !key.starts_with(&prefix) || !valid_key(&key) {
            continue;
        }
        let _lock = lock(&store, &key)?;
        if entry.path().try_exists()? {
            File::create(entry.path().join("retired"))?;
            collect(&entry.path())?;
        }
    }
    Ok(())
}

pub async fn recover(container_dir: &Path, state: &StateManager) -> Result<()> {
    if !container_dir.try_exists()? {
        return Ok(());
    }
    for entry in fs::read_dir(container_dir)? {
        let entry = entry?;
        let id = entry.file_name();
        let Some(id) = id.to_str() else { continue };
        if uuid::Uuid::parse_str(id).is_err()
            || !entry.file_type()?.is_dir()
            || !entry.path().join(OWNER).try_exists()?
        {
            continue;
        }
        if state.get_container_state(id).await?.is_none() {
            remove(&entry.path())?;
            let output = container_dir.join(format!("{id}-out"));
            if output.try_exists()? {
                fs::remove_dir_all(output)?;
            }
        }
        // Live workloads can outlive the daemon. Leave their mounts alone.
        // A stopped or created workload remounts its upper on the next Start.
    }
    let store = container_dir.join(STORE);
    if store.try_exists()? {
        for entry in fs::read_dir(&store)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(key) = name
                .strip_prefix(".building-")
                .or_else(|| name.strip_prefix(".deleting-"))
            {
                let key = key.split_once('.').map_or(key, |(key, _)| key);
                if valid_key(key) {
                    let _lock = lock(&store, key)?;
                    if entry.path().try_exists()? {
                        fs::remove_dir_all(entry.path())?;
                    }
                }
            } else if valid_key(&name) {
                let _lock = lock(&store, &name)?;
                collect(&entry.path())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn failed_publication_is_retryable_and_legacy_bundles_remain_plain() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("container");
        fs::create_dir_all(bundle.join("rootfs")).unwrap();
        let key = key("config", b"manifest");
        let err = prepare(&bundle, &key, &[], |root| {
            fs::write(root.join("partial"), b"uncommitted")?;
            Err(invalid("interrupted extraction"))
        });
        assert!(err.is_err());
        assert!(!temp.path().join(STORE).join(&key).exists());
        assert!(!bundle.join(PIN).exists());
        with_view(&bundle, || Ok(())).unwrap();
        remove(&bundle).unwrap();
        remove(&bundle).unwrap();
    }

    #[tokio::test]
    async fn recovery_only_removes_owned_orphans_and_unpublished_trees() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("bundles");
        let orphan = dir.join(uuid::Uuid::new_v4().to_string());
        let legacy = dir.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(orphan.join("rootfs")).unwrap();
        fs::write(orphan.join(OWNER), "").unwrap();
        fs::create_dir_all(legacy.join("rootfs")).unwrap();
        let staging = dir
            .join(STORE)
            .join(format!(".building-{}", key("cfg", b"m")));
        fs::create_dir_all(&staging).unwrap();
        let interrupted_pull = staging.with_extension("partial");
        fs::create_dir(&interrupted_pull).unwrap();
        let state = StateManager::new(temp.path().join("state.db")).unwrap();
        recover(&dir, &state).await.unwrap();
        assert!(!staging.exists());
        assert!(!interrupted_pull.exists());
        assert!(!orphan.exists());
        assert!(legacy.join("rootfs").exists());
    }

    #[test]
    #[ignore = "requires Linux CAP_SYS_ADMIN and an OverlayFS-capable filesystem"]
    fn concurrent_views_survive_remount_and_retirement() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join(STORE);
        let bundles = [temp.path().join("a"), temp.path().join("b")];
        let count = AtomicUsize::new(0);
        let key = key("config", b"manifest");
        std::thread::scope(|scope| {
            for bundle in &bundles {
                fs::create_dir_all(bundle.join("rootfs")).unwrap();
                scope.spawn(|| {
                    prepare(bundle, &key, &["nosuid".into()], |root| {
                        count.fetch_add(1, Ordering::SeqCst);
                        fs::write(root.join("file"), b"base")?;
                        fs::hard_link(root.join("file"), root.join("alias"))?;
                        fs::write(root.join("deleted"), b"base")?;
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let root = bundles[0].join("rootfs");
        with_view(&bundles[0], || {
            fs::write(root.join("file"), b"copied up")?;
            assert_eq!(fs::read(root.join("alias"))?, b"copied up");
            fs::remove_file(root.join("deleted"))?;
            Ok(())
        })
        .unwrap();
        with_view(&bundles[1], || {
            assert_eq!(fs::read(bundles[1].join("rootfs/file"))?, b"base");
            assert!(bundles[1].join("rootfs/deleted").is_file());
            Ok(())
        })
        .unwrap();
        // An open directory on the mounted view makes umount return EBUSY.
        let (tx, rx) = std::sync::mpsc::sync_channel(0);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let _handle = rx.recv().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(35));
            });
            with_view(&bundles[0], || {
                tx.send(File::open(&root)?).unwrap();
                Ok(())
            })
            .unwrap();
        });
        retire(temp.path(), "config").unwrap();
        with_view(&bundles[0], || {
            assert_eq!(fs::read(root.join("file"))?, b"copied up");
            assert!(!root.join("deleted").exists());
            Ok(())
        })
        .unwrap();
        remove(&bundles[0]).unwrap();
        assert!(store.join(&key).is_dir());
        remove(&bundles[1]).unwrap();
        assert!(!store.join(key).exists());
    }
}
