//! OCI spec security assembly — pure functions, unit-tested in
//! `tests/security_spec_test.rs`.
//!
//! CRI `LinuxContainerSecurityContext` maps onto the spec here, with
//! daemon-level `[security]` defaults as fallback and docker-parity
//! seccomp profile reference resolution:
//! - absolute path      -> used as-is
//! - `localhost/<name>`  -> `<seccomp_profile_root>/<name>` (kubelet layout)
//! - `runtime/default`   -> built-in default profile (embedded JSON)

use crate::sandbox::ContainerSecurity;
use serde_json::{json, Value};

/// Resolve a CRI seccomp profile reference to a concrete file path.
/// Returns `None` when no filter should be installed.
pub fn resolve_seccomp_path(requested: &str, profile_root: &str) -> Option<String> {
    let requested = requested.trim();
    if requested.is_empty() {
        return None;
    }
    if requested == "runtime/default" || requested == "docker/default" {
        return None; // caller installs the embedded default profile
    }
    if let Some(name) = requested.strip_prefix("localhost/") {
        if name.contains("..") {
            tracing::warn!(profile = %requested, "rejecting seccomp profile path traversal");
            return None;
        }
        return Some(format!("{}/{}", profile_root.trim_end_matches('/'), name));
    }
    if requested.starts_with('/') {
        return Some(requested.to_string());
    }
    tracing::warn!(profile = %requested, "unsupported seccomp profile ref; ignoring");
    None
}

/// Embedded `runtime/default` profile: a compact SCMP_ACT_ERRNO allowlist
/// covering the syscalls real workloads (busybox/nginx/redis) need on
/// x86_64 and aarch64. Deliberately conservative but not exhaustive.
pub const DEFAULT_SECCOMP_PROFILE: &str = include_str!("default_seccomp.json");

/// Apply CRI security context + daemon defaults onto an OCI spec object.
///
/// Precedence per field: per-container request > daemon default.
/// Seccomp: request path > request `runtime/default` (embedded) >
/// daemon-configured path. Failures to read a profile are logged and
/// skipped — matching the fail-open posture of the engine elsewhere.
pub fn apply_security(
    spec: &mut Value,
    security: Option<&ContainerSecurity>,
    defaults: &crate::core::config::SecurityConfig,
) {
    if let Some(process) = spec.get_mut("process").and_then(|p| p.as_object_mut()) {
        let nnp = security.map_or(defaults.no_new_privileges, |s| s.no_new_privs);
        process.insert("noNewPrivileges".to_string(), json!(nnp));
    }
    if let Some(root) = spec.get_mut("root").and_then(|r| r.as_object_mut()) {
        let ro_final = security.map_or(defaults.read_only_root_filesystem, |s| s.readonly_rootfs)
            && !security.map_or(false, |s| s.privileged);
        root.insert("readonly".to_string(), json!(ro_final));
    }

    // Process user / groups. The `user` key may be absent from the base
    // spec; always create it so uid/gid overrides land even when the image
    // config omitted them.
    if let Some(process) = spec.get_mut("process").and_then(|p| p.as_object_mut()) {
        if security.is_some() {
            let user = process
                .entry("user".to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .expect("user object");
            let sec = security.unwrap();
            if let Some(uid) = sec.run_as_uid {
                user.insert("uid".to_string(), json!(uid));
            }
            if let Some(gid) = sec.run_as_gid {
                user.insert("gid".to_string(), json!(gid));
            }
            if !sec.supplemental_groups.is_empty() {
                let sg: Vec<serde_json::Value> = sec
                    .supplemental_groups
                    .iter()
                    .map(|g| json!(g))
                    .collect();
                user.insert("supplementaryGroups".to_string(), json!(sg));
            }
        }
    }

    // Privileged containers: every capability, unrestricted.
    let privileged = security.map_or(false, |s| s.privileged);
    if privileged {        if let Some(process) = spec.get_mut("process").and_then(|p| p.as_object_mut()) {
            process.insert("noNewPrivileges".to_string(), json!(false));
            process.remove("apparmorProfile");
            process.remove("selinuxLabel");
            let caps = process
                .entry("capabilities".to_string())
                .or_insert_with(|| json!({}));
            let caps = caps.as_object_mut().expect("capabilities object");
            for set_name in ["effective", "bounding", "permitted", "inheritable", "ambient"] {
                let arr: Vec<serde_json::Value> =
                    ALL_CAPS.iter().map(|c| json!(c)).collect();
                caps.insert(set_name.to_string(), json!(arr));
            }
        }
    }

    // Capabilities (default set in runc_backend config; adjust per request).
    {
        let normalize_cap = |c: &str| -> String {
            if c.starts_with("CAP_") { c.to_string() } else { format!("CAP_{c}") }
        };
        let adds: Vec<String> = security
            .map(|s| s.add_capabilities.clone())
            .unwrap_or_default()
            .iter()
            .map(|c| normalize_cap(c))
            .collect();
        // `ALL`/`CAP_ALL` in add_capabilities means the full set; a literal
        // "ALL" (or our CAP_-normalized CAP_ALL) fails OCI validation, so
        // expand it here.
        let adds: Vec<String> = if adds.iter().any(|c| c == "ALL" || c == "CAP_ALL") {
            ALL_CAPS.iter().map(|c| c.to_string()).collect()
        } else {
            adds
        };
        let drops: Vec<String> = security
            .map(|s| s.drop_capabilities.clone())
            .unwrap_or_default()
            .iter()
            .map(|c| normalize_cap(c))
            .collect();
        if !adds.is_empty() || !drops.is_empty() {
            if let Some(process) = spec.get_mut("process").and_then(|p| p.as_object_mut()) {
                let caps = process
                    .entry("capabilities".to_string())
                    .or_insert_with(|| json!({}));
                let caps = caps.as_object_mut().expect("capabilities object");
                let mut effective: Vec<serde_json::Value> = caps
                    .get("effective")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_else(|| json!(["CAP_NET_BIND_SERVICE"]).as_array().unwrap().clone());
                let drop_all = drops.iter().any(|d| d == "ALL" || d == "CAP_ALL");
                if drop_all {
                    effective.clear();
                    for (_, v) in caps.iter_mut() {
                        *v = json!([]);
                    }
                } else {
                    effective.retain(|c| {
                        !drops.iter().any(|d| c.as_str() == Some(d.as_str()))
                    });
                    for a in &adds {
                        effective.push(json!(a));
                    }
                }
                caps.insert("effective".to_string(), json!(effective.clone()));
                if !drop_all {
                    // Adds must appear in every capability set libcontainer
                    // re-derives from (bounding/permitted/inheritable/ambient),
                    // otherwise its drop_privileges step fails with
                    // "failed to drop capabilities" (permitted must contain
                    // the effective set).
                    for set_name in ["bounding", "permitted", "inheritable", "ambient"] {
                        let set = caps
                            .entry(set_name.to_string())
                            .or_insert_with(|| json!([]));
                        if let Some(arr) = set.as_array_mut() {
                            arr.retain(|c| !drops.iter().any(|d| c.as_str() == Some(d.as_str())));
                            for a in &adds {
                                if !arr.iter().any(|c| c.as_str() == Some(a.as_str())) {
                                    arr.push(json!(a));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // The runtime applies these after entering the container rootfs. Paths
    // absent from its procfs are permitted by the OCI runtime specification.
    if let Some(security) = security {
        if !security.masked_paths.is_empty() {
            spec["linux"]["maskedPaths"] = json!(security.masked_paths);
        }
        if !security.readonly_paths.is_empty() {
            spec["linux"]["readonlyPaths"] = json!(security.readonly_paths);
        }
    }

    // Seccomp profile selection (skipped for privileged).
    let sec = security.and_then(|s| {
        if s.privileged { None } else {
            (!s.seccomp_profile_path.is_empty()).then(|| s.seccomp_profile_path.clone())
        }
    });
    let requested = sec;
    match requested.as_deref() {
        Some("unconfined") | Some("") => {} // no filter
        Some("runtime/default") | Some("docker/default") => {
            match serde_json::from_str::<Value>(DEFAULT_SECCOMP_PROFILE) {
                Ok(v) => embed_seccomp(spec, v),
                Err(e) => tracing::warn!(error = %e, "embedded seccomp profile invalid"),
            }
        }
        Some(abs @ other) if other.starts_with('/') => {
            embed_from_file(spec, abs);
        }
        Some(other) => {
            if let Some(path) = resolve_seccomp_path(other, &defaults.seccomp_profile_root) {
                embed_from_file(spec, &path);
            }
        }
        None => {
            if !defaults.seccomp_profile_path.is_empty() && !privileged {
                embed_from_file(spec, &defaults.seccomp_profile_path);
            }
        }
    }

    // AppArmor: install the requested profile. Privileged containers remove
    // apparmorProfile above, so ordering (privileged first) is preserved.
    if !privileged {
        if let Some(sec) = security {
            apply_apparmor(spec, &sec.apparmor_profile);
        }
    }
}

fn embed_from_file(spec: &mut Value, path: &str) {
    match std::fs::read_to_string(path) {
        Ok(txt) => match serde_json::from_str::<Value>(&txt) {
            Ok(v) => embed_seccomp(spec, v),
            Err(e) => tracing::warn!(path = %path, error = %e, "seccomp profile parse failed; continuing without"),
        },
        Err(e) => tracing::warn!(path = %path, error = %e, "seccomp profile unreadable; continuing without"),
    }
}

fn embed_seccomp(spec: &mut Value, profile: Value) {
    if let Some(linux) = spec.get_mut("linux").and_then(|l| l.as_object_mut()) {
        linux.insert("seccomp".to_string(), profile);
    }
}

/// True when the named AppArmor profile is currently loaded on the node
/// (checked via /sys/kernel/security/apparmor/profiles, no privileges needed).
/// True when the named AppArmor profile is loaded on the node.
/// /sys/kernel/security/apparmor/profiles is the preferred source; some
/// hosts (restricted securityfs) need aa-status instead — both are readable
/// by the root-running daemon.
pub fn apparmor_profile_loaded(name: &str) -> bool {
    if name.is_empty()
        || name == "unconfined"
        || name == "runtime/default"
        || name == "docker/default"
    {
        return true;
    }
    if let Ok(txt) = std::fs::read_to_string("/sys/kernel/security/apparmor/profiles") {
        return txt.lines().any(|l| l.starts_with(&format!("{name} (")));
    }
    if let Ok(out) = std::process::Command::new("aa-status").output() {
        let txt = String::from_utf8_lossy(&out.stdout);
        return txt.lines().any(|l| l.trim().starts_with(name));
    }
    false
}

/// Install the request's AppArmor profile on the spec's process node
/// (`apparmorProfile`, consumed by libcontainer/runc). \`runtime/default\`
/// and \`unconfined\` mean 'no profile' for AppArmor — the node is omitted.
pub fn apply_apparmor(spec: &mut Value, profile: &str) {
    if profile.is_empty()
        || profile == "unconfined"
        || profile == "runtime/default"
        || profile == "docker/default"
    {
        return;
    }
    if let Some(process) = spec.get_mut("process").and_then(|p| p.as_object_mut()) {
        process.insert("apparmorProfile".to_string(), json!(profile));
    }
}

/// The standard Linux capability names (41, incl. optional newer ones).
const ALL_CAPS: [&str; 41] = [
    "CAP_AUDIT_CONTROL","CAP_AUDIT_READ","CAP_AUDIT_WRITE","CAP_BLOCK_SUSPEND",
    "CAP_BPF","CAP_CHECKPOINT_RESTORE","CAP_CHOWN","CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH","CAP_FOWNER","CAP_FSETID","CAP_IPC_LOCK",
    "CAP_IPC_OWNER","CAP_KILL","CAP_LEASE","CAP_LINUX_IMMUTABLE","CAP_MAC_ADMIN",
    "CAP_MAC_OVERRIDE","CAP_MKNOD","CAP_NET_ADMIN","CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST","CAP_NET_RAW","CAP_PERFMON","CAP_SETFCAP","CAP_SETGID",
    "CAP_SETPCAP","CAP_SETUID","CAP_SYSLOG","CAP_SYS_ADMIN","CAP_SYS_BOOT",
    "CAP_SYS_CHROOT","CAP_SYS_MODULE","CAP_SYS_NICE","CAP_SYS_PACCT",
    "CAP_SYS_PTRACE","CAP_SYS_RAWIO","CAP_SYS_RESOURCE","CAP_SYS_TIME",
    "CAP_SYS_TTY_CONFIG","CAP_WAKE_ALARM",
];
