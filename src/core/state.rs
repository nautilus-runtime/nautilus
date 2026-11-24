use crate::core::Result;
use serde::{Deserialize, Serialize};
use sled::Db;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerState {
    pub id: String,
    pub pod_id: String,
    #[serde(default)]
    pub attempt: u32,
    pub name: String,
    pub image: String,
    pub status: ContainerStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub exit_code: Option<i32>,
    pub pid: Option<u32>,
    pub log_path: Option<String>,
    /// CRI runtime_handler this container executes on ("runc", "wasi", ...).
    #[serde(default)]
    pub sandbox_kind: String,
    /// Resource limits requested via CRI (mapped to OCI linux.resources).
    #[serde(default)]
    pub resources: Option<crate::sandbox::ResourceLimits>,
    /// Per-container security context (CRI LinuxContainerSecurityContext).
    #[serde(default)]
    pub security: Option<crate::sandbox::ContainerSecurity>,
    /// Environment variables requested via CRI (merged over image config env).
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub labels: HashMap<String, String>,
    pub annotations: HashMap<String, String>,
    /// Host-path volumes from CRI ContainerConfig.mounts.
    #[serde(default)]
    pub volumes: Vec<crate::sandbox::VolumeMount>,
    /// CRI ContainerStatus.reason (e.g. "OOMKilled").
    #[serde(default)]
    pub finish_reason: String,
    /// CRI ContainerConfig.stdin: the init's stdin must stay OPEN (daemon-held
    /// pipe) so stdin-oriented workloads (attach shells) don't EOF-exit
    /// before a client attaches.
    #[serde(default)]
    pub open_stdin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm: Option<VmContainer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmContainer {
    #[serde(default)]
    pub image_id: String,
    pub preparing: bool,
    #[serde(default)]
    pub logs_complete: bool,
    pub relative_log: String,
    pub cursor: crate::runtime::vm_log::Cursor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmPodAttempt {
    pub attempt: u32,
    #[serde(default)]
    pub ready_network: Option<crate::network::vm::VmNetwork>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PodState {
    pub id: String,
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub status: PodStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub containers: Vec<String>,
    /// Resolved CRI runtime_handler for this pod's sandboxes.
    #[serde(default)]
    pub runtime_handler: String,
    /// Present for fully managed CRI VM attempts, including pending cleanup.
    #[serde(default)]
    pub vm_attempt: Option<VmPodAttempt>,
    /// Internal startup probe; kubelet must never enumerate this sandbox.
    #[serde(default)]
    pub vm_probe: bool,
    /// Exact CRI cgroup parent assigned by kubelet.
    #[serde(default)]
    pub cgroup_parent: String,
    /// Published Pod netns bind-mount path.
    #[serde(default)]
    pub network_namespace: Option<String>,
    /// Pod IP assigned by the CNI chain.
    #[serde(default)]
    pub network_ip: String,
    /// Host directory that the kubelet/critest expects per-pod logs under
    /// (`/var/log/pods/<ns>_<name>_<uid>/`); container LogPath resolves inside
    /// it. Empty when the CRI request carried none.
    #[serde(default)]
    pub log_directory: String,
    /// Sandbox hostname (CRI).
    #[serde(default)]
    pub hostname: String,
    /// DNS nameservers from the sandbox config (CRI).
    #[serde(default)]
    pub dns_servers: Vec<String>,
    /// DNS search domains from the sandbox config (CRI).
    #[serde(default)]
    pub dns_searches: Vec<String>,
    /// DNS resolver options from the sandbox config (CRI).
    #[serde(default)]
    pub dns_options: Vec<String>,
    /// Holder process keeping the netns alive.
    #[serde(default)]
    pub netns_holder_pid: Option<i32>,
    /// Holder process of the pod's dedicated IPC namespace (when pod sysctls
    /// require one); containers join it by the deterministic handle path.
    #[serde(default)]
    pub ipc_holder_pid: Option<i32>,
    /// Parked init (pid 1) of the pod's shared PID namespace; containers with
    /// pid mode POD join it by the handle path. None for host-pid pods.
    #[serde(default)]
    pub pidns_holder_pid: Option<i32>,
    /// Sandbox asked for a privileged pod; containers inherit full caps.
    #[serde(default)]
    pub privileged: bool,
    /// Sandbox runs hostNetwork (namespace_options.network == NODE): no CNI
    /// netns is created and containers stay on the host network stack.
    #[serde(default)]
    pub host_network: bool,
    /// Sandbox namespace_options pid/ipc == NODE: containers keep the host
    /// pid/ipc namespaces instead of fresh ones (CRI pod-scoped semantics).
    #[serde(default)]
    pub host_pid: bool,
    #[serde(default)]
    pub host_ipc: bool,
    pub labels: HashMap<String, String>,
    pub annotations: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ContainerStatus {
    Created,
    Running,
    Stopped,
    Paused,
    Unknown,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub enum PodStatus {
    #[default]
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Debug)]
pub struct StateManager {
    db: Arc<RwLock<Db>>,
}

impl StateManager {
    /// Create a new StateManager with the given database path
    pub fn new<P: AsRef<Path>>(db_path: P) -> Result<Self> {
        let db = sled::open(db_path)?;
        Ok(Self {
            db: Arc::new(RwLock::new(db)),
        })
    }

    /// Generate a new unique container ID
    pub fn generate_container_id() -> String {
        Uuid::new_v4().to_string()
    }

    /// Generate a new unique pod ID
    pub fn generate_pod_id() -> String {
        Uuid::new_v4().to_string()
    }

    /// Save container state to database
    pub async fn save_container_state(&self, state: &ContainerState) -> Result<()> {
        let db = self.db.read().await;
        let key = format!("container:{}", state.id);
        let value = serde_json::to_vec(state)?;
        db.insert(key.as_bytes(), value)?;
        db.flush_async().await?;
        Ok(())
    }

    /// Get container state from database
    pub async fn get_container_state(&self, id: &str) -> Result<Option<ContainerState>> {
        let db = self.db.read().await;
        let key = format!("container:{}", id);
        if let Some(value) = db.get(key.as_bytes())? {
            let state: ContainerState = serde_json::from_slice(&value)?;
            Ok(Some(state))
        } else {
            Ok(None)
        }
    }

    /// Delete container state from database
    pub async fn delete_container_state(&self, id: &str) -> Result<()> {
        let db = self.db.read().await;
        let key = format!("container:{}", id);
        db.remove(key.as_bytes())?;
        db.flush_async().await?;
        Ok(())
    }

    /// Save pod state to database
    pub async fn save_pod_state(&self, state: &PodState) -> Result<()> {
        let db = self.db.read().await;
        let key = format!("pod:{}", state.id);
        let value = serde_json::to_vec(state)?;
        db.insert(key.as_bytes(), value)?;
        db.flush_async().await?;
        Ok(())
    }

    /// Get pod state from database
    pub async fn get_pod_state(&self, id: &str) -> Result<Option<PodState>> {
        let db = self.db.read().await;
        let key = format!("pod:{}", id);
        if let Some(value) = db.get(key.as_bytes())? {
            let state: PodState = serde_json::from_slice(&value)?;
            Ok(Some(state))
        } else {
            Ok(None)
        }
    }

    /// Delete pod state from database
    pub async fn delete_pod_state(&self, id: &str) -> Result<()> {
        let db = self.db.read().await;
        let key = format!("pod:{}", id);
        db.remove(key.as_bytes())?;
        db.flush_async().await?;
        Ok(())
    }

    /// List all containers
    pub async fn list_containers(&self) -> Result<Vec<ContainerState>> {
        let db = self.db.read().await;
        let mut containers = Vec::new();
        
        for result in db.scan_prefix(b"container:") {
            let (_, value) = result?;
            let state: ContainerState = serde_json::from_slice(&value)?;
            containers.push(state);
        }
        
        Ok(containers)
    }

    /// List all pods
    pub async fn list_pods(&self) -> Result<Vec<PodState>> {
        let db = self.db.read().await;
        let mut pods = Vec::new();
        
        for result in db.scan_prefix(b"pod:") {
            let (_, value) = result?;
            let state: PodState = serde_json::from_slice(&value)?;
            pods.push(state);
        }
        
        Ok(pods)
    }

    /// A single undecodable Pod must not prevent other VMs from being recovered at startup.
    pub(crate) async fn vm_pods_for_recovery(&self) -> Result<Vec<PodState>> {
        let db = self.db.read().await;
        let mut attempts = Vec::new();
        for entry in db.scan_prefix(b"pod:") {
            let (key, value) = entry?;
            let pod: PodState = match serde_json::from_slice(&value) {
                Ok(pod) => pod,
                Err(error) => {
                    tracing::warn!(key=%String::from_utf8_lossy(&key), %error, "Skipping unreadable Pod during VM recovery");
                    continue;
                }
            };
            if pod.runtime_handler == "vm" && (pod.vm_probe || pod.status != PodStatus::Succeeded) {
                if key.strip_prefix(b"pod:") == Some(pod.id.as_bytes()) {
                    attempts.push(pod);
                } else {
                    tracing::warn!(key=%String::from_utf8_lossy(&key), "Pod ID does not match its state key");
                }
            }
        }
        Ok(attempts)
    }

    /// List containers in a specific pod
    pub async fn list_containers_in_pod(&self, pod_id: &str) -> Result<Vec<ContainerState>> {
        let containers = self.list_containers().await?;
        Ok(containers.into_iter()
            .filter(|c| c.pod_id == pod_id)
            .collect())
    }

    /// Update container status
    pub async fn update_container_status(&self, id: &str, status: ContainerStatus) -> Result<()> {
        if let Some(mut state) = self.get_container_state(id).await? {
            state.status = status;
            
            // Update timestamps based on status
            match state.status {
                ContainerStatus::Running => {
                    if state.started_at.is_none() {
                        state.started_at = Some(chrono::Utc::now());
                    }
                }
                ContainerStatus::Stopped => {
                    state.finished_at = Some(chrono::Utc::now());
                }
                _ => {}
            }
            
            self.save_container_state(&state).await?;
        }
        Ok(())
    }

    /// Update pod status
    pub async fn update_pod_status(&self, id: &str, status: PodStatus) -> Result<()> {
        if let Some(mut state) = self.get_pod_state(id).await? {
            state.status = status;
            self.save_pod_state(&state).await?;
        }
        Ok(())
    }
}