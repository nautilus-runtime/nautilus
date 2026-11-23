//! TDD coverage for pod-sandbox sysctl application (src/network/sysctl.rs).
//! PodSandboxConfig.Linux.Sysctls must land inside the pod's new network
//! namespace (written by the forked holder after unshare), and keys must be
//! sanitized against path traversal.

use nautilus::network::sysctl::{apply_sysctls, validate_sysctl_key};

#[test]
fn accepts_dotted_kernel_and_net_keys() {
    assert!(validate_sysctl_key("kernel.shm_rmid_forced").is_ok());
    assert!(validate_sysctl_key("net.core.somaxconn").is_ok());
    assert!(validate_sysctl_key("net.ipv4.ip_forward").is_ok());
}

#[test]
fn rejects_traversal_and_junk() {
    assert!(validate_sysctl_key("..").is_err());
    assert!(validate_sysctl_key("../etc/evil").is_err());
    assert!(validate_sysctl_key("kernel..shm").is_err());
    assert!(validate_sysctl_key("").is_err());
    assert!(validate_sysctl_key("with space").is_err());
    assert!(validate_sysctl_key("UPPER.case").is_err());
    assert!(validate_sysctl_key("-lead.ing").is_err());
}

#[test]
fn apply_writes_proc_files_with_value_and_newline() {
    let proc = tempfile::tempdir().unwrap();
    // Fake the /proc/sys tree for two knobs.
    for dir in ["kernel", "net/core"] {
        std::fs::create_dir_all(proc.path().join("sys").join(dir)).unwrap();
    }
    let sysctls = vec![
        ("kernel.shm_rmid_forced".to_string(), "1".to_string()),
        ("net.core.somaxconn".to_string(), "1024".to_string()),
    ];
    let n = apply_sysctls(&proc.path().join("sys"), &sysctls).unwrap();
    assert_eq!(n, 2);
    assert_eq!(
        std::fs::read_to_string(proc.path().join("sys/kernel/shm_rmid_forced")).unwrap(),
        "1\n"
    );
    assert_eq!(
        std::fs::read_to_string(proc.path().join("sys/net/core/somaxconn")).unwrap(),
        "1024\n"
    );
}

#[test]
fn apply_rejects_traversal_without_touching_fs() {
    let proc = tempfile::tempdir().unwrap();
    let sysctls = vec![("../etc/evil".to_string(), "1".to_string())];
    assert!(apply_sysctls(&proc.path().join("sys"), &sysctls).is_err());
    // Nothing escaped the tree.
    assert!(
        !proc.path().join("etc").exists(),
        "traversal must not create paths outside /proc/sys root"
    );
}

#[test]
fn apply_reports_missing_knob_as_error() {
    let proc = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(proc.path().join("sys")).unwrap();
    let sysctls = vec![("kernel.does_not_exist_xyz".to_string(), "1".to_string())];
    assert!(apply_sysctls(&proc.path().join("sys"), &sysctls).is_err());
}
