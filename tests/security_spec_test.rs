//! TDD coverage for OCI spec security assembly (src/sandbox/spec.rs).
//! Red-green: every branch of the CRI security context mapping is asserted
//! here before/while it is implemented.

use nautilus::core::config::SecurityConfig;
use nautilus::sandbox::spec::{apply_security, resolve_seccomp_path, DEFAULT_SECCOMP_PROFILE};
use nautilus::sandbox::ContainerSecurity;
use serde_json::{json, Value};

fn base_spec() -> Value {
    json!({
        "process": {"cwd": "/"},
        "root": {"path": "rootfs", "readonly": false},
        "linux": {"namespaces": []}
    })
}

fn defaults() -> SecurityConfig {
    let mut d = SecurityConfig::default();
    d.no_new_privileges = true;
    d.read_only_root_filesystem = false;
    d.seccomp_profile_path = String::new();
    d.seccomp_profile_root = "/var/lib/kubelet/seccomp".to_string();
    d
}

#[test]
fn daemon_default_no_new_privs_applies_when_request_absent() {
    let mut spec = base_spec();
    apply_security(&mut spec, None, &defaults());
    assert_eq!(spec["process"]["noNewPrivileges"], json!(true));
    assert_eq!(spec["root"]["readonly"], json!(false));
}

#[test]
fn request_overrides_daemon_default() {
    let mut spec = base_spec();
    let sec = ContainerSecurity {
        no_new_privs: false,
        readonly_rootfs: true,
        seccomp_profile_path: String::new(),
        apparmor_profile: String::new(),
        ..Default::default()
    };
    apply_security(&mut spec, Some(&sec), &defaults());
    assert_eq!(spec["process"]["noNewPrivileges"], json!(false));
    assert_eq!(spec["root"]["readonly"], json!(true));
}

#[test]
fn runtime_default_installs_embedded_profile() {
    let mut spec = base_spec();
    let sec = ContainerSecurity {
        no_new_privs: true,
        readonly_rootfs: false,
        seccomp_profile_path: "runtime/default".to_string(),
        apparmor_profile: String::new(),
        ..Default::default()
    };
    apply_security(&mut spec, Some(&sec), &defaults());
    assert_eq!(spec["process"]["noNewPrivileges"], json!(true));
    let prof = &spec["linux"]["seccomp"];
    assert_eq!(prof["defaultAction"], "SCMP_ACT_ERRNO");
    // The allowlist must be non-trivial or nothing would run.
    let n = prof["syscalls"].as_array().map(|a| a.len()).unwrap_or(0);
    assert!(n > 0, "embedded default profile has no allowlist entries");
}

#[test]
fn embedded_default_profile_is_valid_and_shaped() {
    let v: Value = serde_json::from_str(DEFAULT_SECCOMP_PROFILE)
        .expect("embedded profile must be valid JSON");
    assert_eq!(v["defaultAction"], "SCMP_ACT_ERRNO");
    // The adopted moby-maintained profile mixes ALLOW entries with targeted
    // ERRNO/TRACE ones; every entry just needs a known action and names.
    const KNOWN_ACTIONS: [&str; 5] = [
        "SCMP_ACT_ALLOW",
        "SCMP_ACT_ERRNO",
        "SCMP_ACT_TRACE",
        "SCMP_ACT_KILL",
        "SCMP_ACT_LOG",
    ];
    for entry in v["syscalls"].as_array().expect("syscalls array") {
        let action = entry["action"].as_str().expect("entry action");
        assert!(
            KNOWN_ACTIONS.contains(&action),
            "unknown seccomp action {action}"
        );
        assert!(
            entry["names"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
            "every entry needs names"
        );
    }
    // Workload-critical syscalls must be in the ALLOW set.
    let all: Vec<&str> = v["syscalls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "SCMP_ACT_ALLOW")
        .filter_map(|e| e["names"].as_array())
        .flatten()
        .filter_map(|n| n.as_str())
        .collect();
    for needed in ["openat", "execve", "wait4", "clone", "epoll_wait", "futex"] {
        assert!(all.contains(&needed), "default profile must allow {needed}");
    }
}

#[test]
fn localhost_profile_resolves_under_kubelet_root() {
    assert_eq!(
        resolve_seccomp_path("localhost/profile.json", "/var/lib/kubelet/seccomp"),
        Some("/var/lib/kubelet/seccomp/profile.json".to_string())
    );
}

#[test]
fn localhost_profile_traversal_is_rejected() {
    assert_eq!(
        resolve_seccomp_path("localhost/../../etc/shadow", "/var/lib/kubelet/seccomp"),
        None
    );
}

#[test]
fn absolute_profile_path_passes_through() {
    assert_eq!(
        resolve_seccomp_path("/opt/profiles/strict.json", "/var/lib/kubelet/seccomp"),
        Some("/opt/profiles/strict.json".to_string())
    );
}

#[test]
fn unknown_scheme_is_ignored() {
    assert_eq!(resolve_seccomp_path("foobar/default", "/x"), None);
    assert_eq!(resolve_seccomp_path("", "/x"), None);
}

#[test]
fn localhost_profile_gets_embedded_from_disk() {
    // Arrange a profile file under a temp kubelet-style root.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("seccomp");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("blocks-rename.json"),
        r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["rename"],"action":"SCMP_ACT_ERRNO"}]}"#,
    )
    .unwrap();

    let mut d = defaults();
    d.seccomp_profile_root = root.display().to_string();

    let mut spec = base_spec();
    let sec = ContainerSecurity {
        no_new_privs: true,
        readonly_rootfs: false,
        seccomp_profile_path: "localhost/blocks-rename.json".to_string(),
        apparmor_profile: String::new(),
        ..Default::default()
    };
    apply_security(&mut spec, Some(&sec), &d);
    assert_eq!(
        spec["linux"]["seccomp"]["syscalls"][0]["names"][0],
        json!("rename")
    );
}

#[test]
fn unreadable_profile_fails_open_without_touching_spec() {
    let mut spec = base_spec();
    let mut d = defaults();
    d.seccomp_profile_path = "/definitely/not/here.json".to_string();
    apply_security(&mut spec, None, &d);
    assert!(spec["linux"].get("seccomp").is_none(), "no filter installed");
}

#[test]
fn apparmor_profile_lands_on_process_when_requested() {
    let mut spec = base_spec();
    let sec = ContainerSecurity {
        no_new_privs: true,
        readonly_rootfs: false,
        seccomp_profile_path: String::new(),
        apparmor_profile: "docker-default".to_string(),
        ..Default::default()
    };
    apply_security(&mut spec, Some(&sec), &defaults());
    assert_eq!(spec["process"]["apparmorProfile"], json!("docker-default"));
}

#[test]
fn unconfined_or_empty_apparmor_is_omitted() {
    for profile in ["", "unconfined"] {
        let mut spec = base_spec();
        let sec = ContainerSecurity {
            no_new_privs: true,
            readonly_rootfs: false,
            seccomp_profile_path: String::new(),
            apparmor_profile: profile.to_string(),
            ..Default::default()
        };
        apply_security(&mut spec, Some(&sec), &defaults());
        assert!(
            spec["process"].get("apparmorProfile").is_none(),
            "profile {profile:?} must not add apparmorProfile"
        );
    }
}

#[test]
fn privileged_removes_apparmor_profile() {
    let mut spec = base_spec();
    let sec = ContainerSecurity {
        no_new_privs: false,
        readonly_rootfs: false,
        seccomp_profile_path: "runtime/default".to_string(),
        apparmor_profile: "some-profile".to_string(),
        privileged: true,
        add_capabilities: vec![],
        drop_capabilities: vec![],
        run_as_uid: None,
        run_as_gid: None,
        run_as_name: String::new(),
        supplemental_groups: vec![],
        masked_paths: vec![],
        readonly_paths: vec![],
        host_network: false,
        host_pid: false,
        host_ipc: false,
        share_pod_pid: false,
    };
    apply_security(&mut spec, Some(&sec), &defaults());
    assert!(
        spec["process"].get("apparmorProfile").is_none(),
        "privileged must not carry an apparmor profile"
    );
}
