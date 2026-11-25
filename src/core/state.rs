use crate::core::Result;
use serde::{Deserialize, Serialize};
use sled::Db;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerState {
    pub id: String,
    pub pod_id: String,
    #[serde(default)]
    pub attempt: u32,
    pub name: String,
    pub image: String,
    /// Config digest of the manifest that supplied the prepared OCI rootfs.
    #[serde(default)]
    pub image_id: String,
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
    /// Arguments for a WASI command, restored when it starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wasi_args: Vec<String>,
    pub labels: HashMap<String, String>,
    pub annotations: HashMap<String, String>,
    /// Host-path volumes from CRI ContainerConfig.mounts.
    #[serde(default)]
    pub volumes: Vec<crate::sandbox::VolumeMount>,
    /// CRI ContainerStatus.reason (e.g. "OOMKilled").
    #[serde(default)]
    pub finish_reason: String,
    /// Keep init stdin open before the first attach.
    #[serde(default)]
    pub open_stdin: bool,
    #[serde(default)]
    pub stdin_once: bool,
    #[serde(default)]
    pub tty: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm: Option<VmContainer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmContainer {
    // Older Pod VM records kept the config digest here.
    #[serde(default, skip_serializing_if = "String::is_empty")]
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
    #[serde(default)]
    pub attempt: u32,
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
    /// Pod created by the standalone Nautilus client on the shared CRI socket.
    #[serde(default)]
    pub standalone: bool,
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
    pod_containers: Arc<Mutex<PodContainers>>,
    container_events: broadcast::Sender<Arc<ContainerEvent>>,
    all_container_events: broadcast::Sender<Arc<ContainerEvent>>,
}

#[derive(Debug, Default)]
struct PodContainers(HashMap<String, BTreeSet<String>>);

impl PodContainers {
    fn update(&mut self, prior: Option<&ContainerState>, next: Option<&ContainerState>) {
        if matches!((prior, next), (Some(prior), Some(next)) if prior.pod_id == next.pod_id) {
            return;
        }
        if let Some(prior) = prior {
            if let Some(containers) = self.0.get_mut(&prior.pod_id) {
                containers.remove(&prior.id);
                if containers.is_empty() {
                    self.0.remove(&prior.pod_id);
                }
            }
        }
        if let Some(next) = next {
            self.0.entry(next.pod_id.clone()).or_default().insert(next.id.clone());
        }
    }
}

pub(crate) const CONTAINER_EVENT_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerEventKind {
    Created,
    Started,
    Stopped,
    Deleted,
}

#[derive(Debug)]
pub(crate) struct ContainerEvent {
    pub id: String,
    pub kind: ContainerEventKind,
    pub created_at: i64,
    pub pod: Option<PodState>,
    pub containers: Vec<ContainerState>,
}

fn published_workload(state: &ContainerState) -> bool {
    state.vm.as_ref().is_none_or(|vm| !vm.preparing)
}

fn container_event_kinds(
    prior: Option<&ContainerState>,
    state: Option<&ContainerState>,
) -> Vec<ContainerEventKind> {
    use ContainerEventKind::{Created, Deleted, Started, Stopped};

    let prior = prior.filter(|state| published_workload(state));
    let Some(state) = state.filter(|state| published_workload(state)) else {
        return prior.map_or_else(Vec::new, |_| vec![Deleted]);
    };
    if prior.is_none() && state.status == ContainerStatus::Created {
        return vec![Created];
    }

    let mut events = Vec::new();
    let started =
        state.started_at.is_some() && prior.and_then(|s| s.started_at) != state.started_at;
    if started
        && matches!(
            state.status,
            ContainerStatus::Running | ContainerStatus::Stopped
        )
    {
        events.push(Started);
    }
    let lost_guest = state.status == ContainerStatus::Unknown
        && state.started_at.is_some()
        && state.vm.as_ref().is_some_and(|vm| vm.logs_complete)
        && prior.is_some_and(|s| s.vm.as_ref().is_some_and(|vm| !vm.logs_complete));
    if lost_guest
        || (state.status == ContainerStatus::Stopped
            && (started || prior.is_none_or(|s| s.status != ContainerStatus::Stopped)))
    {
        events.push(Stopped);
    }
    events
}

fn publish_container_events(
    db: &Db,
    pod_containers: &PodContainers,
    sender: &broadcast::Sender<Arc<ContainerEvent>>,
    all_sender: &broadcast::Sender<Arc<ContainerEvent>>,
    prior: Option<&ContainerState>,
    state: Option<&ContainerState>,
) -> Result<()> {
    let kinds = container_event_kinds(prior, state);
    if kinds.is_empty() {
        return Ok(());
    }
    let changed = state.or(prior).expect("an event has a container record");
    let pod: Option<PodState> = db
        .get(format!("pod:{}", changed.pod_id))?
        .map(|value| serde_json::from_slice(&value))
        .transpose()?;
    if pod.as_ref().is_some_and(|pod| pod.vm_probe) {
        return Ok(());
    }
    let cri_visible = !pod.as_ref().is_some_and(|pod| pod.standalone);
    if all_sender.receiver_count() == 0 && (!cri_visible || sender.receiver_count() == 0) {
        return Ok(());
    }

    let mut containers = Vec::new();
    for id in pod_containers.0.get(&changed.pod_id).into_iter().flatten() {
        if let Some(value) = db.get(format!("container:{id}"))? {
            let container: ContainerState = serde_json::from_slice(&value)?;
            if published_workload(&container) {
                containers.push(container);
            }
        }
    }
    let mut kinds = kinds.into_iter().peekable();
    while let Some(kind) = kinds.next() {
        let mut snapshot = if kinds.peek().is_none() {
            std::mem::take(&mut containers)
        } else {
            containers.clone()
        };
        if kind == ContainerEventKind::Started && changed.status == ContainerStatus::Stopped {
            // A guest Start may already have observed the command's exit.
            if let Some(container) = snapshot.iter_mut().find(|c| c.id == changed.id) {
                container.status = ContainerStatus::Running;
                container.finished_at = None;
                container.exit_code = None;
                container.finish_reason.clear();
            }
        }
        let event = Arc::new(ContainerEvent {
            id: changed.id.clone(),
            kind,
            created_at: chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
            pod: pod.clone(),
            containers: snapshot,
        });
        if cri_visible {
            let _ = sender.send(event.clone());
        }
        let _ = all_sender.send(event);
    }
    Ok(())
}

impl StateManager {
    /// Create a new StateManager with the given database path
    pub fn new<P: AsRef<Path>>(db_path: P) -> Result<Self> {
        let db = sled::open(db_path)?;
        // Pod state can lag CreateContainer while the committed workload is already visible.
        let mut pod_containers = PodContainers::default();
        for entry in db.scan_prefix(b"container:") {
            let (_, value) = entry?;
            if let Ok(container) = serde_json::from_slice(&value) {
                pod_containers.update(None, Some(&container));
            }
        }
        Ok(Self {
            db: Arc::new(RwLock::new(db)),
            pod_containers: Arc::new(Mutex::new(pod_containers)),
            container_events: broadcast::channel(CONTAINER_EVENT_CAPACITY).0,
            all_container_events: broadcast::channel(CONTAINER_EVENT_CAPACITY).0,
        })
    }

    pub(crate) fn subscribe_container_events(&self) -> broadcast::Receiver<Arc<ContainerEvent>> {
        self.container_events.subscribe()
    }

    pub(crate) fn subscribe_all_container_events(&self) -> broadcast::Receiver<Arc<ContainerEvent>> {
        self.all_container_events.subscribe()
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
        self.write_container(&state.id, Some(state)).await
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
        self.write_container(id, None).await
    }

    async fn write_container(&self, id: &str, state: Option<&ContainerState>) -> Result<()> {
        // Keep the commit and its snapshot in order even if the RPC disconnects.
        let db = self.db.clone().write_owned().await;
        let pod_containers = self.pod_containers.clone();
        let events = self.container_events.clone();
        let all_container_events = self.all_container_events.clone();
        let key = format!("container:{}", id);
        let state = state.cloned();
        tokio::task::spawn_blocking(move || {
            let prior = match &state {
                Some(state) => db.insert(key, serde_json::to_vec(state)?)?,
                None => db.remove(key)?,
            };
            let prior = prior.and_then(|value| serde_json::from_slice(&value).ok());
            let mut pod_containers = pod_containers.lock().unwrap();
            pod_containers.update(prior.as_ref(), state.as_ref());
            db.flush()?;
            if events.receiver_count() != 0 || all_container_events.receiver_count() != 0 {
                let published = publish_container_events(&db, &pod_containers, &events, &all_container_events, prior.as_ref(), state.as_ref());
                if let Err(error) = published {
                    tracing::warn!(%error, "could not read committed container event snapshot");
                }
            }
            Ok(())
        }).await.map_err(|e| crate::core::Error::Runtime(format!("container state commit task: {e}")))?
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

#[cfg(test)]
mod event_tests {
    use super::*;

    fn sample_container(id: &str, pod_id: &str, vm: Option<VmContainer>) -> ContainerState {
        ContainerState {
            id: id.into(),
            pod_id: pod_id.into(),
            attempt: 0,
            name: "c".into(),
            image: "img".into(),
            image_id: String::new(),
            status: ContainerStatus::Created,
            created_at: chrono::Utc::now(),
            started_at: None,
            finished_at: None,
            exit_code: None,
            pid: None,
            log_path: None,
            sandbox_kind: if vm.is_some() { "vm" } else { "runc" }.into(),
            resources: None,
            security: None,
            env: HashMap::new(),
            wasi_args: Vec::new(),
            labels: HashMap::new(),
            annotations: HashMap::new(),
            volumes: Vec::new(),
            finish_reason: String::new(),
            open_stdin: false,
            stdin_once: false,
            tty: false,
            vm,
        }
    }

    #[test]
    fn vm_preparing_fast_exit_and_lost_guest_transitions() {
        use ContainerEventKind::{Created, Deleted, Started, Stopped};

        let mut vm_ctr = sample_container(
            "c1",
            "p1",
            Some(VmContainer {
                image_id: String::new(),
                preparing: true,
                logs_complete: false,
                relative_log: "c.log".into(),
                cursor: Default::default(),
            }),
        );
        vm_ctr.status = ContainerStatus::Unknown;
        assert!(container_event_kinds(None, Some(&vm_ctr)).is_empty());

        // Rollback while preparing emits nothing.
        assert!(container_event_kinds(Some(&vm_ctr), None).is_empty());

        // Publish created container.
        let mut created = vm_ctr.clone();
        created.status = ContainerStatus::Created;
        created.vm.as_mut().unwrap().preparing = false;
        assert_eq!(
            container_event_kinds(Some(&vm_ctr), Some(&created)),
            vec![Created]
        );

        // Fast-exit guest observed directly in Stopped on Start emits Started then Stopped.
        let mut fast_exited = created.clone();
        fast_exited.status = ContainerStatus::Stopped;
        fast_exited.started_at = Some(chrono::Utc::now());
        fast_exited.finished_at = Some(chrono::Utc::now());
        fast_exited.exit_code = Some(35);
        assert_eq!(
            container_event_kinds(Some(&created), Some(&fast_exited)),
            vec![Started, Stopped]
        );

        // Repeat stop emits nothing.
        assert!(container_event_kinds(Some(&fast_exited), Some(&fast_exited)).is_empty());

        // Running -> Unknown (temporary disconnect) emits nothing; then logs_complete emits Stopped.
        let mut running = created.clone();
        running.status = ContainerStatus::Running;
        running.started_at = Some(chrono::Utc::now());
        let mut disconnected = running.clone();
        disconnected.status = ContainerStatus::Unknown;
        assert!(container_event_kinds(Some(&running), Some(&disconnected)).is_empty());
        let mut lost = disconnected.clone();
        lost.vm.as_mut().unwrap().logs_complete = true;
        assert_eq!(
            container_event_kinds(Some(&disconnected), Some(&lost)),
            vec![Stopped]
        );
        assert!(container_event_kinds(Some(&lost), Some(&lost)).is_empty());

        // Deleting a published container emits Deleted.
        assert_eq!(container_event_kinds(Some(&lost), None), vec![Deleted]);
    }

    #[tokio::test]
    async fn vm_probe_pod_does_not_emit_events() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = StateManager::new(tmp.path().join("state")).unwrap();
        let mut rx = manager.subscribe_container_events();
        let probe_pod = PodState {
            id: "probe-pod".into(),
            standalone: false,
            name: "probe".into(),
            namespace: "system".into(),
            uid: "probe-uid".into(),
            attempt: 0,
            status: PodStatus::Running,
            created_at: chrono::Utc::now(),
            containers: Vec::new(),
            network_namespace: None,
            network_ip: String::new(),
            netns_holder_pid: None,
            pidns_holder_pid: None,
            ipc_holder_pid: None,
            cgroup_parent: String::new(),
            hostname: String::new(),
            dns_servers: Vec::new(),
            dns_searches: Vec::new(),
            dns_options: Vec::new(),
            log_directory: String::new(),
            labels: HashMap::new(),
            annotations: HashMap::new(),
            runtime_handler: "vm".into(),
            host_network: false,
            host_pid: false,
            host_ipc: false,
            privileged: false,
            vm_attempt: None,
            vm_probe: true,
        };
        manager.save_pod_state(&probe_pod).await.unwrap();
        let ctr = sample_container("probe-ctr", "probe-pod", None);
        manager.save_container_state(&ctr).await.unwrap();
        manager.delete_container_state("probe-ctr").await.unwrap();
        assert!(rx.try_recv().is_err());
    }
}
