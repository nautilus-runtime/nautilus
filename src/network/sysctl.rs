//! Pod-sandbox sysctls: validated application inside the freshly-created
//! network namespace (the forked holder writes them after `unshare`), so
//! netns-scoped knobs (net.core.*, net.ipv4.*) take effect for the pod.

use crate::{Error, Result};
use std::path::Path;

/// A sysctl key must be lowercase dotted path segments — it becomes a
/// relative path under /proc/sys, so anything else is a traversal risk.
pub fn validate_sysctl_key(key: &str) -> Result<()> {
    let ok = !key.is_empty()
        && key
            .split('.')
            .all(|seg| {
                !seg.is_empty()
                    && seg.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            });
    if ok {
        Ok(())
    } else {
        Err(Error::Network(format!("invalid sysctl key: {key}")))
    }
}

/// Write each key/value under `<proc_sys_root>/<dotted.key with dots as />`.
/// Returns the number of applied sysctls; first failure aborts (the holder
/// then exits non-zero and pod setup fails loudly rather than silently
/// ignoring requested tuning).
pub fn apply_sysctls(proc_sys_root: &Path, sysctls: &[(String, String)]) -> Result<usize> {
    let mut applied = 0usize;
    for (key, value) in sysctls {
        validate_sysctl_key(key)?;
        let rel = key.replace('.', "/");
        let path = proc_sys_root.join(&rel);
        std::fs::write(&path, format!("{value}\n"))
            .map_err(|e| Error::Network(format!("sysctl {key}={value}: {e}")))?;
        applied += 1;
    }
    Ok(applied)
}

/// Which namespace instance a sysctl is scoped to — decides WHERE the
/// holder must write it for containers to observe it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Per-network-namespace (net.*).
    Net,
    /// Per-IPC-namespace (kernel.shm*, kernel.msg*, kernel.sem, fs.mqueue.*).
    Ipc,
    /// Global on the host.
    Other,
}

pub fn scope_of(key: &str) -> Scope {
    if key.starts_with("net.") {
        Scope::Net
    } else if key.starts_with("fs.mqueue.")
        || key.starts_with("kernel.msg")
        || key.starts_with("kernel.sem")
        || key.starts_with("kernel.shm")
    {
        Scope::Ipc
    } else {
        Scope::Other
    }
}
