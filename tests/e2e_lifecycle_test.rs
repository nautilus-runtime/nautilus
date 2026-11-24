//! End-to-end lifecycle tests for Nautilus CRI
//! Tests the complete lifecycle of pods, containers, and images

use nautilus::core::{Config, StateManager};
use nautilus::core::config::{PodConfig, ContainerConfig};
use nautilus::cri::{NautilusRuntimeService, NautilusImageService};
use nautilus::runtime::{ContainerManager, PodManager};
use nautilus::storage::ImageManager;
use nautilus::network::NetworkManager;
use nautilus::proto::runtime::v1::runtime_service_server::RuntimeService;
use nautilus::proto::runtime::v1::image_service_server::ImageService;
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;
use tokio;
use uuid::Uuid;

/// E2E test for complete pod lifecycle
#[tokio::test]
async fn test_e2e_pod_lifecycle() {
    let test_env = setup_test_environment().await;
    
    // Test pod creation
    let pod_name = format!("test-pod-{}", Uuid::new_v4());
    let pod_config = create_test_pod_config(&pod_name, &test_env.temp_dir);
    
    // Create pod through CRI service
    let create_result = RuntimeService::run_pod_sandbox(
        &*test_env.runtime_service,
        tonic::Request::new(nautilus::proto::runtime::v1::RunPodSandboxRequest {
            config: Some(pod_config_to_proto(&pod_config)),
            runtime_handler: String::new(),
        })
    ).await;
    
    let pod_id = match create_result {
        Ok(response) => {
            let created_pod_id = response.into_inner().pod_sandbox_id;
            println!("✓ Pod created successfully: {}", created_pod_id);
            created_pod_id
        }
        Err(e) => panic!("pod sandbox creation failed: {e}"),
    };
    
    // Test pod status query
    let status_result = test_env.pod_manager.get_pod_state(&pod_id).await;
    assert!(status_result.is_ok());
    
    if let Ok(Some(pod_state)) = status_result {
        assert_eq!(pod_state.id, pod_id);
        assert_eq!(pod_state.name, pod_name);
        println!("✓ Pod status verified: {:?}", pod_state.status);
    }
    
    // Test pod listing
    let pods = test_env.pod_manager.list_pods().await.unwrap_or_default();
    let found_pod = pods.iter().find(|p| p.id == pod_id);
    if found_pod.is_some() {
        println!("✓ Pod found in listing");
    }
    
    // Cleanup - remove pod
    let remove_result = test_env.pod_manager.remove_pod(&pod_id).await;
    if remove_result.is_ok() {
        println!("✓ Pod removed successfully");
    }
}

/// E2E test for complete container lifecycle
#[tokio::test]
async fn test_e2e_container_lifecycle() {
    let test_env = setup_test_environment().await;
    
    // First create a pod
    let pod_name = format!("test-pod-{}", Uuid::new_v4());
    let pod_config = create_test_pod_config(&pod_name, &test_env.temp_dir);
    
    let pod_create_result = test_env.pod_manager.create_pod(
        &pod_config.namespace,
        &pod_config.name,
        &pod_config.id,
        "runc",
        pod_config.labels.clone(),
        pod_config.annotations.clone(),
        "",
        "",
        &[],
        &[],
        &[],
        false,
        false,
        false,
        false,
        None,
    ).await;
    
    let pod_id = match pod_create_result {
        Ok(id) => id,
        Err(e) => panic!("pod creation failed: {e}"),
    };
    
    // Test container creation
    let container_id = format!("test-container-{}", Uuid::new_v4());
    let container_config = create_test_container_config(&container_id, &test_env.temp_dir);
    
    let create_result = test_env.container_manager.create_container(
        &pod_id,
        &container_config.name,
        &container_config.image,
        "runc",
        container_config.command.clone(),
        container_config.args.clone(),
        "",
        vec![],
        std::collections::HashMap::new(),
        None,
        None,
        container_config.labels.clone(),
        container_config.annotations.clone(),
        false,
    ).await;
    
    match create_result {
        Ok(_) => {
            println!("✓ Container created successfully: {}", container_id);
        }
        Err(e) => panic!("container creation failed: {e}"),
    }
    
    // Test container status query
    let status_result = test_env.container_manager.get_container_state(&container_id).await;
    assert!(status_result.is_ok());
    
    if let Ok(Some(container_state)) = status_result {
        assert_eq!(container_state.id, container_id);
        assert_eq!(container_state.pod_id, pod_id);
        println!("✓ Container status verified: {:?}", container_state.status);
    }
    
    // Test container listing
    let containers = test_env.container_manager.list_containers().await.unwrap_or_default();
    let found_container = containers.iter().find(|c| c.id == container_id);
    if found_container.is_some() {
        println!("✓ Container found in listing");
    }
    
    // Test listing containers in pod
    let pod_containers = test_env.container_manager.list_containers_in_pod(&pod_id).await.unwrap_or_default();
    let found_in_pod = pod_containers.iter().find(|c| c.id == container_id);
    if found_in_pod.is_some() {
        println!("✓ Container found in pod listing");
    }
    
    // Cleanup - remove container first, then pod
    let container_remove_result = test_env.container_manager.remove_container(&container_id).await;
    if container_remove_result.is_ok() {
        println!("✓ Container removed successfully");
    }
    
    let pod_remove_result = test_env.pod_manager.remove_pod(&pod_id).await;
    if pod_remove_result.is_ok() {
        println!("✓ Pod removed successfully");
    }
}

/// E2E test for image lifecycle
#[tokio::test]
async fn test_e2e_image_lifecycle() {
    let test_env = setup_test_environment().await;
    
    // Test image listing
    let list_result = test_env.image_manager.list_images(None).await;
    match list_result {
        Ok(images) => {
            println!("✓ Image listing successful: {} images found", images.len());
            
            // If we have images, test status query for the first one
            if let Some(first_image) = images.first() {
                let status_result = test_env.image_manager.get_image_status(&first_image.id).await;
                match status_result {
                    Ok(Some(_status)) => {
                        println!("✓ Image status query successful for: {}", first_image.id);
                    }
                    Ok(None) => panic!(
                        "image status returned None for {}",
                        first_image.id
                    ),
                    Err(e) => panic!("image status query failed: {e}"),
                }
            }
        }
        Err(e) => panic!("image listing failed: {e}"),
    }
    
    // Test filesystem info
    let fs_result = test_env.image_manager.get_fs_info().await;
    match fs_result {
        Ok((total, used)) => {
            println!("✓ Filesystem info: {} total, {} used", total, used);
            assert!(total >= used, "Total space should be >= used space");
        }
        Err(e) => panic!("filesystem info failed: {e}"),
    }
    
    // Test image pull (this will likely fail in test environment)
    let test_image = "alpine:latest";
    let pull_result = ImageService::pull_image(
        &*test_env.image_service,
        tonic::Request::new(nautilus::proto::runtime::v1::PullImageRequest {
            image: Some(nautilus::proto::runtime::v1::ImageSpec {
                image: test_image.to_string(),
                user_specified_image: test_image.to_string(),
                annotations: HashMap::new(),
                runtime_handler: String::new(),
                image_ref: String::new(),
            }),
            auth: None,
            sandbox_config: None,
        })
    ).await;
    
    match pull_result {
        Ok(response) => {
            let image_ref = response.into_inner().image_ref;
            println!("✓ Image pulled successfully: {}", image_ref);
            
            // Test image removal
            let remove_result = ImageService::remove_image(
                &*test_env.image_service,
                tonic::Request::new(nautilus::proto::runtime::v1::RemoveImageRequest {
                    image: Some(nautilus::proto::runtime::v1::ImageSpec {
                        image: image_ref.clone(),
                        user_specified_image: image_ref.clone(),
                        annotations: HashMap::new(),
                        runtime_handler: String::new(),
                        image_ref: String::new(),
                    }),
                })
            ).await;
            
            match remove_result {
                Ok(_) => {
                    println!("✓ Image removed successfully: {}", image_ref);
                }
                Err(e) => panic!("image removal failed: {e}"),
            }
        }
        Err(e) => panic!("image pull failed: {e}"),
    }
}

/// Test state persistence across restarts
#[tokio::test]
async fn test_e2e_state_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let state_dir = temp_dir.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let db_path = state_dir.join("persistence_test.db");
    
    let pod_id = format!("persistent-pod-{}", Uuid::new_v4());
    let container_id = format!("persistent-container-{}", Uuid::new_v4());
    
    // Create and save state
    {
        let state_manager = StateManager::new(db_path.clone()).unwrap();
        
        let pod_state = nautilus::core::PodState {
            vm_attempt: None,
            id: pod_id.clone(),
            name: "persistent-test-pod".to_string(),
            namespace: "default".to_string(),
            uid: "test-uid".to_string(),
            status: nautilus::core::PodStatus::Running,
            created_at: chrono::Utc::now(),
            containers: vec![container_id.clone()],
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
            labels: HashMap::new(),
            annotations: HashMap::new(),
        };

        let container_state = nautilus::core::ContainerState {
            id: container_id.clone(),
            name: "persistent-test-container".to_string(),
            pod_id: pod_id.clone(),
            image: "alpine:latest".to_string(),
            env: HashMap::new(),
            status: nautilus::core::ContainerStatus::Running,
            created_at: chrono::Utc::now(),
            started_at: Some(chrono::Utc::now()),
            finished_at: None,
            exit_code: None,
            finish_reason: String::new(),
            pid: Some(1234),
            log_path: Some("/tmp/container.log".to_string()),
            sandbox_kind: "runc".to_string(),
            resources: None,
            security: None,
            volumes: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
            open_stdin: false,
            vm: None,
        };
        
        state_manager.save_pod_state(&pod_state).await.unwrap();
        state_manager.save_container_state(&container_state).await.unwrap();
        
        println!("✓ State saved successfully");
    }
    
    // Recreate state manager and verify persistence
    {
        let state_manager = StateManager::new(db_path).unwrap();
        
        let pod_state = state_manager.get_pod_state(&pod_id).await.unwrap();
        assert!(pod_state.is_some(), "Pod state should be persisted");
        
        let pod_state = pod_state.unwrap();
        assert_eq!(pod_state.id, pod_id);
        assert_eq!(pod_state.name, "persistent-test-pod");
        
        let container_state = state_manager.get_container_state(&container_id).await.unwrap();
        assert!(container_state.is_some(), "Container state should be persisted");
        
        let container_state = container_state.unwrap();
        assert_eq!(container_state.id, container_id);
        assert_eq!(container_state.pod_id, pod_id);
        
        println!("✓ State persistence verified");
    }
}

/// Test network integration
#[tokio::test]
async fn test_e2e_network_integration() {
    let test_env = setup_test_environment().await;
    
    // Test network manager initialization
    let init_result = test_env.network_manager.initialize().await;
    match init_result {
        Ok(_) => {
            println!("✓ Network manager initialized successfully");
        }
        Err(e) => panic!("network manager initialization failed: {e}"),
    }
    
    // Test network listing
    let networks_result = test_env.network_manager.list_networks().await;
    match networks_result {
        Ok(networks) => {
            println!("✓ Network listing successful: {} networks found", networks.len());
        }
        Err(e) => panic!("network listing failed: {e}"),
    }
    
    // Test default network creation
    let network_name = format!("test-network-{}", Uuid::new_v4());
    let create_result = test_env.network_manager.create_network(
        &network_name,
        "bridge",
        Some("172.20.0.0/16"),
        Some("172.20.0.1")
    ).await;
    let network_id = match create_result {
        Ok(id) => {
            println!("✓ Network created successfully: {} ({})", network_name, id);
            id
        }
        Err(e) => panic!("network creation failed: {e}"),
    };

    // Docker parity: removal accepts both the id and its name.
    test_env.network_manager.remove_network(&network_id).await
        .unwrap_or_else(|e| panic!("network removal by id failed: {e}"));
    // The record must be gone; a by-name retry must fail.
    assert!(
        test_env.network_manager.remove_network(&network_name).await.is_err(),
        "network should be gone after removal"
    );
    println!("✓ Network removed (by id) and by-name lookup now fails as expected");
}

// Helper functions

struct TestEnvironment {
    temp_dir: TempDir,
    _config: Arc<Config>,
    _state_manager: Arc<StateManager>,
    pod_manager: Arc<PodManager>,
    container_manager: Arc<ContainerManager>,
    image_manager: Arc<ImageManager>,
    network_manager: Arc<NetworkManager>,
    runtime_service: Arc<NautilusRuntimeService>,
    image_service: Arc<NautilusImageService>,
}

async fn setup_test_environment() -> TestEnvironment {
    let temp_dir = TempDir::new().unwrap();
    let state_dir = temp_dir.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    
    let mut config = Config::default();
    config.runtime.state_dir = state_dir.clone();
    config.storage.root_dir = temp_dir.path().join("storage");
    config.storage.image_dir = temp_dir.path().join("images");
    config.storage.container_dir = temp_dir.path().join("containers");
    config.storage.tmp_dir = temp_dir.path().join("tmp");
    
    // Create storage directories
    std::fs::create_dir_all(&config.storage.root_dir).unwrap();
    std::fs::create_dir_all(&config.storage.image_dir).unwrap();
    std::fs::create_dir_all(&config.storage.container_dir).unwrap();
    std::fs::create_dir_all(&config.storage.tmp_dir).unwrap();
    
    let config = Arc::new(config);
    
    let db_path = state_dir.join("test.db");
    let state_manager = Arc::new(StateManager::new(db_path).unwrap());
    
    let sandbox_registry = Arc::new(nautilus::sandbox::SandboxRegistry::new(&config).unwrap());
    let container_manager = Arc::new(ContainerManager::new(
        config.clone(),
        state_manager.clone(),
        sandbox_registry,
    ));
    
    let pod_manager = Arc::new(PodManager::new(
        config.clone(),
        state_manager.clone(),
        container_manager.clone(),
    ));
    
    let image_manager = Arc::new(ImageManager::new(config.clone()));
    let network_manager = Arc::new(NetworkManager::new(config.clone()));
    
    let runtime_service = Arc::new(NautilusRuntimeService::new(
        config.clone(),
        state_manager.clone(),
    ));
    
    let image_service = Arc::new(NautilusImageService::new(
        config.clone(),
        state_manager.clone(),
    ));
    
    TestEnvironment {
        temp_dir,
        _config: config,
        _state_manager: state_manager,
        pod_manager,
        container_manager,
        image_manager,
        network_manager,
        runtime_service,
        image_service,
    }
}

fn create_test_pod_config(pod_name: &str, temp_dir: &TempDir) -> PodConfig {
    PodConfig {
        id: format!("{}-{}", pod_name, Uuid::new_v4()),
        name: pod_name.to_string(),
        namespace: "default".to_string(),
        uid: 1000,
        gid: 1000,
        hostname: pod_name.to_string(),
        log_directory: temp_dir.path().join("logs"),
        dns_config: None,
        port_mappings: vec![],
        labels: HashMap::new(),
        annotations: HashMap::new(),
    }
}

fn create_test_container_config(container_id: &str, temp_dir: &TempDir) -> ContainerConfig {
    ContainerConfig {
        id: container_id.to_string(),
        name: "test-container".to_string(),
        image: "alpine:latest".to_string(),
        command: vec!["/bin/sh".to_string()],
        args: vec!["-c".to_string(), "sleep 30".to_string()],
        working_dir: "/".to_string(),
        env: HashMap::new(),
        mounts: vec![],
        labels: HashMap::new(),
        annotations: HashMap::new(),
        linux: None,
        log_path: temp_dir.path().join("container.log"),
        stdin: false,
        stdin_once: false,
        tty: false,
    }
}

fn pod_config_to_proto(config: &PodConfig) -> nautilus::proto::runtime::v1::PodSandboxConfig {
    nautilus::proto::runtime::v1::PodSandboxConfig {
        metadata: Some(nautilus::proto::runtime::v1::PodSandboxMetadata {
            name: config.name.clone(),
            uid: config.id.clone(),
            namespace: config.namespace.clone(),
            attempt: 0,
        }),
        hostname: config.hostname.clone(),
        log_directory: config.log_directory.to_string_lossy().to_string(),
        dns_config: None,
        port_mappings: vec![],
        labels: config.labels.clone(),
        annotations: config.annotations.clone(),
        linux: None,
        windows: None,
    }
}