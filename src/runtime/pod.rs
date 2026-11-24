use crate::core::{Config, Error, Result, StateManager};
use crate::core::state::{PodState, PodStatus};
use crate::runtime::ContainerManager;
use futures::{stream, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct PodManager {
    config: Arc<Config>,
    state_manager: Arc<StateManager>,
    container_manager: Arc<ContainerManager>,
    pods: Arc<RwLock<HashMap<String, PodState>>>,
    operations: std::sync::Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
}

impl PodManager {
    pub fn new(
        config: Arc<Config>,
        state_manager: Arc<StateManager>,
        container_manager: Arc<ContainerManager>,
    ) -> Self {
        Self {
            config,
            state_manager,
            container_manager,
            pods: Arc::new(RwLock::new(HashMap::new())),
            operations: Default::default(),
        }
    }

    /// Serialize resource and membership changes for this sandbox across RPCs.
    pub async fn operation_guard(&self, pod_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut operations = self.operations.lock().unwrap();
            operations.retain(|_, lock| lock.strong_count() > 0);
            let slot = operations.entry(pod_id.to_owned()).or_default();
            slot.upgrade().unwrap_or_else(|| {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                *slot = Arc::downgrade(&lock);
                lock
            })
        };
        lock.lock_owned().await
    }

    /// Create a new pod
    pub async fn create_pod(
        &self,
        namespace: &str,
        name: &str,
        uid: &str,
        runtime_handler: &str,
        labels: HashMap<String, String>,
        annotations: HashMap<String, String>,
        log_directory: &str,
        hostname: &str,
        dns_servers: &[String],
        dns_searches: &[String],
        dns_options: &[String],
        privileged: bool,
        host_network: bool,
        host_pid: bool,
        host_ipc: bool,
        pidns_holder_pid: Option<i32>,
    ) -> Result<String> {
        let pod_id = StateManager::generate_pod_id();

        let pod_state = PodState {
            id: pod_id.clone(),
            namespace: namespace.to_string(),
            name: name.to_string(),
            uid: uid.to_string(),
            status: PodStatus::Pending,
            created_at: chrono::Utc::now(),
            containers: Vec::new(),
            network_namespace: None,
            network_ip: String::new(),
            netns_holder_pid: None,
            ipc_holder_pid: None,
            runtime_handler: runtime_handler.to_string(),
            labels,
            annotations,
            log_directory: log_directory.to_string(),
            hostname: hostname.to_string(),
            dns_servers: dns_servers.to_vec(),
            dns_searches: dns_searches.to_vec(),
            dns_options: dns_options.to_vec(),
            privileged,
            host_network,
            host_pid,
            host_ipc,
            pidns_holder_pid,
        };

        // Save to state manager
        self.state_manager.save_pod_state(&pod_state).await?;
        
        // Cache in memory
        {
            let mut pods = self.pods.write().await;
            pods.insert(pod_id.clone(), pod_state);
        }

        tracing::info!(pod_id = %pod_id, namespace = %namespace, name = %name, "Pod created");
        Ok(pod_id)
    }

    /// Start a pod (setup network namespace and prepare for containers)
    pub async fn start_pod(&self, pod_id: &str) -> Result<()> {
        let _guard = self.operation_guard(pod_id).await;
        self.start_pod_locked(pod_id).await
    }

    /// The CRI caller keeps the same guard across network allocation and publication.
    pub(crate) async fn start_pod_locked(&self, pod_id: &str) -> Result<()> {
        let mut pod_state = self.get_pod_state(pod_id).await?
            .ok_or_else(|| Error::Pod(format!("Pod {} not found", pod_id)))?;

        if pod_state.status != PodStatus::Pending {
            return Err(Error::Pod(format!(
                "Pod {} is not in pending state, current status: {:?}",
                pod_id, pod_state.status
            )));
        }

        if pod_state.runtime_handler == "vm" {
            let store = self.vm_store();
            let start = match &self.config.runtime.sandboxes.vm {
                Some(images) => store
                    .start(pod_id, &images.vmm, &images.kernel, &images.initramfs)
                    .await
                    .map_err(Error::from),
                None => Err(Error::Config("Pod VM images are not configured".into())),
            };
            if let Err(error) = start {
                self.reconcile_vm_pod(&store, &pod_state).await?;
                return Err(error);
            }
            // Guest reachability alone does not satisfy RunPodSandbox readiness.
            return Ok(());
        }

        // Network namespace setup lives in the CRI layer: PodNet (podnet.rs)
        // creates the holder + netns + CNI wiring and persists it via
        // set_pod_network(). Wasm pods intentionally stay host-net.
        pod_state.status = PodStatus::Running;

        self.state_manager.save_pod_state(&pod_state).await?;
        
        {
            let mut pods = self.pods.write().await;
            pods.insert(pod_id.to_string(), pod_state);
        }

        tracing::info!(pod_id = %pod_id, "Pod started");
        Ok(())
    }

    /// Stop a pod and all its containers
    pub async fn stop_pod(&self, pod_id: &str, grace_period: Option<u64>) -> Result<()> {
        let _guard = self.operation_guard(pod_id).await;
        self.stop_pod_locked(pod_id, grace_period).await
    }

    pub(crate) async fn stop_pod_locked(
        &self,
        pod_id: &str,
        grace_period: Option<u64>,
    ) -> Result<()> {
        let mut pod_state = self.get_pod_state(pod_id).await?
            .ok_or_else(|| Error::Pod(format!("Pod {} not found", pod_id)))?;

        if pod_state.status != PodStatus::Running && pod_state.runtime_handler != "vm" {
            return Err(Error::Pod(format!(
                "Pod {} is not running, current status: {:?}",
                pod_id, pod_state.status
            )));
        }

        for container in self.container_manager.list_containers_in_pod(pod_id).await? {
            if container.status == crate::core::state::ContainerStatus::Running {
                self.container_manager.stop_container(&container.id, grace_period).await?;
            }
        }
        if pod_state.runtime_handler == "vm" {
            self.vm_store().remove(pod_id).await?;
        }
        pod_state.status = PodStatus::Succeeded;

        self.state_manager.save_pod_state(&pod_state).await?;
        
        {
            let mut pods = self.pods.write().await;
            pods.insert(pod_id.to_string(), pod_state);
        }

        tracing::info!(pod_id = %pod_id, "Pod stopped");
        Ok(())
    }

    /// Remove a pod and all its containers
    pub async fn remove_pod(&self, pod_id: &str) -> Result<()> {
        let _guard = self.operation_guard(pod_id).await;
        self.remove_pod_locked(pod_id).await
    }

    pub(crate) async fn remove_pod_locked(&self, pod_id: &str) -> Result<()> {
        let pod_state = self.get_pod_state(pod_id).await?
            .ok_or_else(|| Error::Pod(format!("Pod {} not found", pod_id)))?;

        if pod_state.status == PodStatus::Running {
            return Err(Error::Pod(format!(
                "Cannot remove running pod {}", pod_id
            )));
        }

        for container in self.container_manager.list_containers_in_pod(pod_id).await? {
            self.container_manager.remove_container(&container.id).await?;
        }
        if pod_state.runtime_handler == "vm" {
            self.vm_store().remove(pod_id).await?;
        }

        // Remove from state manager
        self.state_manager.delete_pod_state(pod_id).await?;
        
        // Remove from memory cache
        {
            let mut pods = self.pods.write().await;
            pods.remove(pod_id);
        }

        tracing::info!(pod_id = %pod_id, "Pod removed");
        Ok(())
    }

    /// Add a container to a pod
    pub async fn add_container_to_pod(&self, pod_id: &str, container_id: &str) -> Result<()> {
        let mut pod_state = self.get_pod_state(pod_id).await?
            .ok_or_else(|| Error::Pod(format!("Pod {} not found", pod_id)))?;

        if !pod_state.containers.contains(&container_id.to_string()) {
            pod_state.containers.push(container_id.to_string());
            
            self.state_manager.save_pod_state(&pod_state).await?;
            
            {
                let mut pods = self.pods.write().await;
                pods.insert(pod_id.to_string(), pod_state);
            }
        }

        Ok(())
    }

    /// Remove a container from a pod
    pub async fn remove_container_from_pod(&self, pod_id: &str, container_id: &str) -> Result<()> {
        let mut pod_state = self.get_pod_state(pod_id).await?
            .ok_or_else(|| Error::Pod(format!("Pod {} not found", pod_id)))?;

        pod_state.containers.retain(|id| id != container_id);
        
        self.state_manager.save_pod_state(&pod_state).await?;
        
        {
            let mut pods = self.pods.write().await;
            pods.insert(pod_id.to_string(), pod_state);
        }

        Ok(())
    }

    /// Get pod state
    pub async fn get_pod_state(&self, pod_id: &str) -> Result<Option<PodState>> {
        // First check memory cache
        {
            let pods = self.pods.read().await;
            if let Some(state) = pods.get(pod_id) {
                return Ok(Some(state.clone()));
            }
        }

        // If not in cache, load from state manager
        if let Some(state) = self.state_manager.get_pod_state(pod_id).await? {
            // Cache it for future use
            {
                let mut pods = self.pods.write().await;
                pods.insert(pod_id.to_string(), state.clone());
            }
            Ok(Some(state))
        } else {
            Ok(None)
        }
    }

    /// List all pods
    pub async fn list_pods(&self) -> Result<Vec<PodState>> {
        self.state_manager.list_pods().await
    }

    pub(crate) fn vm_store(&self) -> crate::vm::owner::PodVmStore {
        crate::vm::owner::PodVmStore::new(self.config.runtime.state_dir.join("vms"))
    }

    /// Reconcile the recorded VM attempts before accepting CRI requests.
    pub async fn recover_vm_pods(&self) -> Result<()> {
        let store = self.vm_store();
        let attempts = self.state_manager.vm_pods_for_recovery().await?;
        // Independent VMs can be queried concurrently. Bound the number of
        // simultaneous system manager requests during a large node restart.
        let mut recovery = stream::iter(attempts)
            .map(|pod| {
                let store = &store;
                async move { self.reconcile_vm_pod(store, &pod).await }
            })
            .buffer_unordered(8);
        while let Some(result) = recovery.next().await {
            result?;
        }
        Ok(())
    }

    async fn reconcile_vm_pod(
        &self,
        store: &crate::vm::owner::PodVmStore,
        pod: &PodState,
    ) -> Result<()> {
        let status = match store.recover(&pod.id).await {
            Ok(Some(_)) => PodStatus::Pending,
            Ok(None) => PodStatus::Failed,
            Err(error) => {
                tracing::warn!(pod_id=%pod.id, %error, "Cannot verify Pod VM ownership");
                PodStatus::Unknown
            }
        };
        if status != pod.status {
            self.update_pod_status(&pod.id, status).await?;
        }
        Ok(())
    }

    /// Get pod status
    pub async fn get_pod_status(&self, pod_id: &str) -> Result<Option<PodStatus>> {
        if let Some(pod_state) = self.get_pod_state(pod_id).await? {
            Ok(Some(pod_state.status))
        } else {
            Ok(None)
        }
    }

    /// Update pod status
    pub async fn update_pod_status(&self, pod_id: &str, status: PodStatus) -> Result<()> {
        if let Some(mut pod_state) = self.get_pod_state(pod_id).await? {
            pod_state.status = status;

            self.state_manager.save_pod_state(&pod_state).await?;

            {
                let mut pods = self.pods.write().await;
                pods.insert(pod_id.to_string(), pod_state);
            }
        }
        Ok(())
    }

    /// Persist the network setup produced by `PodNet::reserve` and `PodNet::attach`.
    pub async fn set_pod_network(
        &self,
        pod_id: &str,
        netns_path: &str,
        ip: &str,
        holder_pid: i32,
    ) -> Result<()> {
        if let Some(mut pod_state) = self.get_pod_state(pod_id).await? {
            pod_state.network_namespace = Some(netns_path.to_string());
            pod_state.network_ip = ip.to_string();
            pod_state.netns_holder_pid = Some(holder_pid);

            self.state_manager.save_pod_state(&pod_state).await?;

            let mut pods = self.pods.write().await;
            pods.insert(pod_id.to_string(), pod_state);
        }
        Ok(())
    }

    /// Persist the pod's shared PID namespace parked-init pid (None clears it).
    pub async fn set_pod_pidns(&self, pod_id: &str, pid: Option<i32>) -> Result<()> {
        if let Some(mut pod_state) = self.get_pod_state(pod_id).await? {
            pod_state.pidns_holder_pid = pid;
            self.state_manager.save_pod_state(&pod_state).await?;
            let mut pods = self.pods.write().await;
            pods.insert(pod_id.to_string(), pod_state);
        }
        Ok(())
    }

    /// Persist the pod IPC namespace holder pid (None clears it).
    pub async fn set_pod_ipc(&self, pod_id: &str, holder_pid: Option<i32>) -> Result<()> {
        if let Some(mut pod_state) = self.get_pod_state(pod_id).await? {
            pod_state.ipc_holder_pid = holder_pid;
            self.state_manager.save_pod_state(&pod_state).await?;
            let mut pods = self.pods.write().await;
            pods.insert(pod_id.to_string(), pod_state);
        }
        Ok(())
    }

    /// Clear the network handle after CNI and namespace cleanup succeed.
    pub async fn clear_pod_network(&self, pod_id: &str) -> Result<()> {
        if let Some(mut pod_state) = self.get_pod_state(pod_id).await? {
            pod_state.network_namespace = None;
            pod_state.network_ip = String::new();
            pod_state.netns_holder_pid = None;

            self.state_manager.save_pod_state(&pod_state).await?;

            let mut pods = self.pods.write().await;
            pods.insert(pod_id.to_string(), pod_state);
        }
        Ok(())
    }
}
