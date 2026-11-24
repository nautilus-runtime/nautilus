use nautilus::core::{Config, StateManager};
use nautilus::core::config::{PodConfig, ContainerConfig};
use nautilus::runtime::{ContainerManager, PodManager};
use nautilus::sandbox::SandboxRegistry;
use std::sync::Arc;
use tempfile::TempDir;
use tokio;

#[tokio::test]
async fn test_pod_and_container_lifecycle() {
    // Create temporary directory for test
    let temp_dir = TempDir::new().unwrap();
    let state_dir = temp_dir.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();

    // Create test config
    let mut config = Config::default();
    config.runtime.state_dir = state_dir.clone();
    config.runtime.runc_path = "/usr/bin/runc".into();
    let config = Arc::new(config);

    // Initialize state manager
    let db_path = state_dir.join("test.db");
    let state_manager = Arc::new(StateManager::new(db_path).unwrap());

    // Initialize managers (container lifecycle goes through the sandbox layer)
    let sandbox_registry = Arc::new(SandboxRegistry::new(&config).unwrap());    let container_manager = Arc::new(ContainerManager::new(
        config.clone(),
        state_manager.clone(),
        sandbox_registry,
    ));
    let pod_manager = Arc::new(PodManager::new(
        config.clone(),
        state_manager.clone(),
        container_manager.clone(),
    ));

    // Test pod creation
    let pod_config = PodConfig {
        id: "test-pod-123".to_string(),
        name: "test-pod".to_string(),
        namespace: "default".to_string(),
        uid: 1000,
        gid: 1000,
        hostname: "test-pod".to_string(),
        log_directory: temp_dir.path().join("logs"),
        dns_config: None,
        port_mappings: vec![],
        labels: std::collections::HashMap::new(),
        annotations: std::collections::HashMap::new(),
    };

    // Create pod
    let result = pod_manager.create_pod(
        &pod_config.namespace,
        &pod_config.name,
        &pod_config.id,
        "wasi",
        pod_config.labels.clone(),
        pod_config.annotations.clone(),
        pod_config.log_directory.to_str().unwrap_or("/tmp"),
        &pod_config.hostname,
        &[],
        &[],
        &[],
        false,
        false,
        false,
        false,
        None,
    ).await;
    assert!(result.is_ok(), "Failed to create pod: {:?}", result.err());
    
    // Get the actual pod_id returned by create_pod
    let pod_id = result.unwrap();
    
    // Verify pod exists
    let pod_state = pod_manager.get_pod_state(&pod_id).await.unwrap();
    assert!(pod_state.is_some(), "Pod state should exist");
    
    let pod_state = pod_state.unwrap();
    assert_eq!(pod_state.id, pod_id);
    assert_eq!(pod_state.name, "test-pod");
    
    // Test container creation
    // A tiny WASI module keeps this test platform-neutral (no docker/runc needed).
    let wat_path = temp_dir.path().join("hello.wat");
    std::fs::write(
        &wat_path,
        r#"(module
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (func (export "_start") i32.const 0 call $exit))"#,
    )
    .unwrap();

    let container_config = ContainerConfig {
        id: "test-container-456".to_string(),
        name: "test-container".to_string(),
        image: format!("file://{}", wat_path.display()),
        command: vec!["/bin/sh".to_string()],
        args: vec!["-c".to_string(), "sleep 30".to_string()],
        working_dir: "/".to_string(),
        env: std::collections::HashMap::new(),
        mounts: vec![],
        labels: std::collections::HashMap::new(),
        annotations: std::collections::HashMap::new(),
        linux: None,
        log_path: temp_dir.path().join("container.log"),
        stdin: false,
        stdin_once: false,
        tty: false,
    };

    // Create container
    let result = container_manager.create_container(
        &pod_id,
        &container_config.name,
        &container_config.image,
        "wasi",
        container_config.command.clone(),
        container_config.args.clone(),
        container_config.log_path.to_str().unwrap_or("container.log"),
        vec![],
        std::collections::HashMap::new(),
        None,
        None,
        container_config.labels.clone(),
        container_config.annotations.clone(),
        false,
    ).await;
    assert!(result.is_ok(), "Failed to create container: {:?}", result.err());
    
    // Get the actual container_id returned by create_container
    let container_id = result.unwrap();
    
    // Verify container exists
    let container_state = container_manager.get_container_state(&container_id).await.unwrap();
    assert!(container_state.is_some(), "Container state should exist");
    
    let container_state = container_state.unwrap();
    assert_eq!(container_state.id, container_id);
    assert_eq!(container_state.pod_id, pod_id);
    
    // List containers
    let containers = container_manager.list_containers().await.unwrap();
    assert_eq!(containers.len(), 1);
    assert_eq!(containers[0].id, container_id);
    
    // List containers in pod
    let pod_containers = container_manager.list_containers_in_pod(&pod_id).await.unwrap();
    assert_eq!(pod_containers.len(), 1);
    assert_eq!(pod_containers[0].id, container_id);
    
    // Clean up - remove container
    let result = container_manager.remove_container(&container_id).await;
    assert!(result.is_ok(), "Failed to remove container: {:?}", result.err());
    
    // Clean up - remove pod
    let result = pod_manager.remove_pod(&pod_id).await;
    assert!(result.is_ok(), "Failed to remove pod: {:?}", result.err());
    
    // Verify cleanup
    let pod_state = pod_manager.get_pod_state(&pod_id).await.unwrap();
    assert!(pod_state.is_none(), "Pod state should be removed");
    
    let container_state = container_manager.get_container_state(&container_id).await.unwrap();
    assert!(container_state.is_none(), "Container state should be removed");
}

#[tokio::test]
async fn test_state_persistence() {
    // Create temporary directory for test
    let temp_dir = TempDir::new().unwrap();
    let state_dir = temp_dir.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    
    let db_path = state_dir.join("persistence_test.db");
    
    // Create and save some state
    {
        let state_manager = StateManager::new(db_path.clone()).unwrap();
        
        let pod_state = nautilus::core::PodState {
            vm_attempt: None,
            id: "persistent-pod".to_string(),
            name: "test-pod".to_string(),
            namespace: "default".to_string(),
            uid: "test-uid".to_string(),
            status: nautilus::core::PodStatus::Running,
            created_at: chrono::Utc::now(),
            containers: vec!["container-1".to_string()],
            network_namespace: None,
            network_ip: String::new(),
            log_directory: String::new(),
            hostname: String::new(),
            dns_servers: vec![],
            dns_searches: vec![],
            dns_options: vec![],
            netns_holder_pid: None,
            ipc_holder_pid: None,
            runtime_handler: "runc".to_string(),
            privileged: false,
            host_network: false,
            host_pid: false,
            host_ipc: false,
            pidns_holder_pid: None,
            labels: std::collections::HashMap::new(),
            annotations: std::collections::HashMap::new(),
        };
        
        state_manager.save_pod_state(&pod_state).await.unwrap();
    }
    
    // Recreate state manager and verify persistence
    {
        let state_manager = StateManager::new(db_path).unwrap();
        let pod_state = state_manager.get_pod_state("persistent-pod").await.unwrap();
        
        assert!(pod_state.is_some());
        let pod_state = pod_state.unwrap();
        assert_eq!(pod_state.id, "persistent-pod");
        assert_eq!(pod_state.name, "test-pod");
        assert_eq!(pod_state.namespace, "default");
    }
}