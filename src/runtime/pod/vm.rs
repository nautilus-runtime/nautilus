//! CRI lifecycle of a networked Pod VM. The caller holds the Pod operation
//! lock. Old owner-only experiments have no VmPodAttempt and stay NOTREADY.
use super::*;
use crate::core::state::VmPodAttempt;
use crate::network::PodNet;
use crate::proto::runtime::v1::DnsConfig;
use crate::vm::{owner::PodVmStore, resources::VmResources};

impl PodManager {
    async fn save_vm_pod(&self, pod: &PodState) -> Result<()> {
        self.state_manager.save_pod_state(pod).await?;
        self.pods.write().await.insert(pod.id.clone(), pod.clone());
        Ok(())
    }

    pub(crate) async fn create_vm_network_locked(
        &self,
        id: &str,
        attempt: u32,
        dns: Option<&DnsConfig>,
        resources: &VmResources,
    ) -> Result<()> {
        let mut pod = self
            .get_pod_state(id)
            .await?
            .ok_or_else(|| Error::Pod("sandbox removed during setup".into()))?;
        let images = self
            .config
            .runtime
            .sandboxes
            .vm
            .as_ref()
            .ok_or_else(|| Error::Config("missing Pod VM images".into()))?;
        if pod.status != PodStatus::Pending || pod.runtime_handler != "vm" {
            return Err(Error::Pod("VM attempt is not pending".into()));
        }
        pod.vm_attempt = Some(VmPodAttempt {
            attempt,
            ready_network: None,
        });
        self.save_vm_pod(&pod).await?;

        let store = self.vm_store();
        let net = PodNet::new(self.config.clone());
        let mut info = net.reserve_vm(id)?;
        let mut nic = net.attach_vm(id, &mut info, dns).await?;
        nic.hostname.clone_from(&pod.hostname);
        store
            .start_with_network(id, images, resources, Some(&nic))
            .await?;
        store.verify_network(id, &nic).await?;
        // The workload agent mounts the share and initializes runc before it
        // listens. Identity can be ready first, so allow this bounded interval.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match store.workload_channel(id).await {
                Ok(_) => break,
                Err(e)
                    if tokio::time::Instant::now() < deadline
                        && matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionRefused
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::UnexpectedEof
                                | std::io::ErrorKind::BrokenPipe
                                | std::io::ErrorKind::TimedOut
                        ) =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await
                }
                Err(e) => return Err(e.into()),
            }
        }
        pod.status = PodStatus::Running;
        pod.network_ip = info.ip;
        pod.network_namespace = Some(info.netns_path);
        pod.vm_attempt.as_mut().unwrap().ready_network = Some(nic);
        self.save_vm_pod(&pod).await
    }

    pub(crate) async fn rollback_vm_locked(&self, id: &str) -> Result<()> {
        self.stop_pod_locked(id, None).await?;
        self.remove_pod_locked(id).await
    }

    pub(crate) async fn observe_vm(&self, pod: &PodState) -> std::io::Result<Option<String>> {
        let store = self.vm_store();
        let boot = store.status(&pod.id).await?;
        if boot.is_some() {
            if let Some(nic) = pod
                .vm_attempt
                .as_ref()
                .and_then(|a| a.ready_network.as_ref())
            {
                store.verify_network(&pod.id, nic).await?;
                store.workload_channel(&pod.id).await?;
            }
        }
        Ok(boot)
    }

    pub(super) async fn mark_vm_stopping(&self, pod: &mut PodState) -> Result<()> {
        let attempt = pod.vm_attempt.as_mut().expect("managed VM Stop");
        attempt.ready_network = None;
        pod.status = PodStatus::Failed;
        self.save_vm_pod(pod).await
    }

    pub(super) async fn recover_managed_vm(
        &self,
        store: &PodVmStore,
        pod: &PodState,
        attempt: &VmPodAttempt,
    ) -> Result<()> {
        let _guard = self.operation_guard(&pod.id).await;
        let net = PodNet::new(self.config.clone());
        if let Some(nic) = &attempt.ready_network {
            match store.recover(&pod.id).await {
                Ok(Some(_)) => {
                    let health = async {
                        store.verify_network(&pod.id, nic).await?;
                        store.workload_channel(&pod.id).await?;
                        std::io::Result::Ok(())
                    }
                    .await;
                    let status = if health.is_ok() {
                        PodStatus::Running
                    } else {
                        PodStatus::Unknown
                    };
                    if status != pod.status {
                        self.update_pod_status(&pod.id, status).await?;
                    }
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(pod_id=%pod.id, %e, "Cannot verify Pod VM ownership");
                    self.update_pod_status(&pod.id, PodStatus::Unknown).await?;
                    return Ok(());
                }
                Ok(None) => {}
            }
        }
        // Clean pending attempts or an exited owned VM. A failure is local
        // to this Pod; retain both journals so kubelet can retry Stop/Remove.
        let mut pod = pod.clone();
        self.mark_vm_stopping(&mut pod).await?;
        let cleanup = async {
            store.remove(&pod.id).await?;
            self.container_manager.guest_pod_lost(&pod.id).await?;
            net.teardown_vm(&pod.id).await?;
            self.clear_pod_network(&pod.id).await
        }
        .await;
        if let Err(e) = cleanup {
            tracing::warn!(pod_id=%pod.id,%e,"VM cleanup retained for retry");
        }
        Ok(())
    }
}
