use nautilus::core::PodState;
use nautilus::proto::runtime::v1::{runtime_service_server::RuntimeService, RemovePodSandboxRequest};
use nautilus::{Config, NautilusRuntimeService, StateManager};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use tonic::Request;

#[tokio::test]
async fn remove_preserves_namespace_and_plugin_identity_until_del_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.network.cni_config_dir = tmp.path().into();
    config.network.cni_bin_dir = tmp.path().into();
    std::fs::write(tmp.path().join("00-test.conf"), r#"{"name":"test","type":"lease"}"#).unwrap();
    let plugin = tmp.path().join("lease");
    std::fs::write(&plugin, r#"#!/bin/sh
cat >/dev/null
echo "$CNI_COMMAND $CNI_CONTAINERID" >> "$CNI_PATH/calls"
test -e "$CNI_PATH/ready"
"#).unwrap();
    std::fs::set_permissions(plugin, std::fs::Permissions::from_mode(0o700)).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let namespace = tmp.path().join("already-unmounted");
    let state = Arc::new(StateManager::new(tmp.path().join("db")).unwrap());
    let pod: PodState = serde_json::from_value(serde_json::json!({
        "id": id, "uid": id, "namespace": "test", "name":"lease", "containers": [],
        "created_at": chrono::Utc::now(), "status": "Running", "runtime_handler":"runc",
        "network_namespace": namespace, "netns_holder_pid": i32::MAX, "labels":{}, "annotations":{}
    })).unwrap();
    state.save_pod_state(&pod).await.unwrap();
    let service = NautilusRuntimeService::new(Arc::new(config), state.clone());
    let remove = || Request::new(RemovePodSandboxRequest { pod_sandbox_id: id.clone() });
    assert!(service.remove_pod_sandbox(remove()).await.is_err());
    let pending = state.get_pod_state(&id).await.unwrap().unwrap();
    assert_eq!(pending.network_namespace, pod.network_namespace);
    assert_eq!(pending.netns_holder_pid, pod.netns_holder_pid);
    std::fs::write(tmp.path().join("ready"), "").unwrap();
    service.remove_pod_sandbox(remove()).await.unwrap();
    assert!(state.get_pod_state(&id).await.unwrap().is_none());
    assert_eq!(std::fs::read_to_string(tmp.path().join("calls")).unwrap(), format!("DEL {id}\nDEL {id}\n"));
}

// Stop journals the VM attempt before shutdown, commits the stopped Pod, and
// Remove deletes its record. These writes may overlap for distinct sandboxes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pod_cleanup_commits_finish_and_survive_reopen() {
    use nautilus::core::PodStatus;
    use std::time::Duration;
    use tokio::sync::Barrier;
    use tokio::task::JoinSet;

    const CHILD_DB: &str = "NAUTILUS_TEST_CONCURRENT_POD_DB";
    let Some(path) = std::env::var_os(CHILD_DB) else {
        // A deadlocked sled pool is process-wide, including Db::drop. Isolate it
        // so a regression fails within the deadline and other tests can continue.
        let tmp = tempfile::tempdir().unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "concurrent_pod_cleanup_commits_finish_and_survive_reopen",
            ])
            .env(CHILD_DB, tmp.path())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        match tokio::time::timeout(Duration::from_secs(15), child.wait()).await {
            Ok(status) => assert!(status.unwrap().success()),
            Err(_) => {
                child.kill().await.unwrap();
                panic!("concurrent Pod state commits stalled");
            }
        }
        let reopened = StateManager::new(tmp.path()).unwrap();
        let pods = reopened.list_pods().await.unwrap();
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].id, "survivor");
        return;
    };

    let state = Arc::new(StateManager::new(path).unwrap());
    let survivor = PodState {
        id: "survivor".into(),
        ..Default::default()
    };
    state.save_pod_state(&survivor).await.unwrap();
    for _ in 0..8 {
        let gate = Arc::new(Barrier::new(16));
        let mut workers = JoinSet::new();
        for _ in 0..16 {
            let state = state.clone();
            let gate = gate.clone();
            workers.spawn(async move {
                let mut pod = PodState {
                    id: uuid::Uuid::new_v4().to_string(),
                    status: PodStatus::Failed,
                    runtime_handler: "vm".into(),
                    ..Default::default()
                };
                gate.wait().await;
                state.save_pod_state(&pod).await.unwrap();
                pod.status = PodStatus::Succeeded;
                state.save_pod_state(&pod).await.unwrap();
                state.delete_pod_state(&pod.id).await.unwrap();
            });
        }
        while let Some(result) = workers.join_next().await {
            result.unwrap();
        }
    }
}
