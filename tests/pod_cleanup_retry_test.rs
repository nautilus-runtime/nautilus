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
