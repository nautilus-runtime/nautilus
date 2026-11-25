use super::{ContainerEventKind, ContainerStatus, NautilusRuntimeService, PodState};
use crate::cri::AUTO_REMOVE_ANNOTATION;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tonic::Status;

fn auto_remove(pod: &PodState) -> bool {
    pod.standalone
        && pod
            .annotations
            .get(AUTO_REMOVE_ANNOTATION)
            .map(String::as_str)
            == Some("true")
}

impl NautilusRuntimeService {
    pub async fn start_auto_removal(&self) {
        let runtime = self.clone();
        let mut events = self._state_manager.subscribe_all_container_events();
        // At startup, no CLI can be between sandbox creation and container creation.
        runtime.reconcile_auto_removals(true).await;
        tokio::spawn(async move {
            let mut retry = tokio::time::interval(Duration::from_secs(30));
            retry.reset();
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Ok(event) if matches!(event.kind, ContainerEventKind::Stopped | ContainerEventKind::Deleted) => {
                            if let Some(pod) = event.pod.as_ref().filter(|pod| auto_remove(pod)) {
                                runtime.try_auto_remove(&pod.id, event.kind == ContainerEventKind::Deleted).await;
                            }
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => runtime.reconcile_auto_removals(false).await,
                        Err(RecvError::Closed) => return,
                    },
                    _ = retry.tick() => runtime.reconcile_auto_removals(false).await,
                }
            }
        });
    }

    async fn reconcile_auto_removals(&self, allow_empty: bool) {
        match self.pod_manager.list_pods().await {
            Ok(pods) => {
                for pod in pods.into_iter().filter(auto_remove) {
                    self.try_auto_remove(&pod.id, allow_empty).await;
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not list standalone Pods for automatic removal")
            }
        }
    }

    async fn try_auto_remove(&self, id: &str, allow_empty: bool) {
        if let Err(error) = self.remove_finished_standalone(id, allow_empty).await {
            tracing::warn!(pod_id = id, %error, "automatic removal will retry");
        }
    }

    async fn remove_finished_standalone(&self, id: &str, allow_empty: bool) -> Result<(), Status> {
        let _guard = self.pod_manager.operation_guard(id).await;
        if !self
            .pod_manager
            .get_pod_state(id)
            .await
            .map_err(Status::from)?
            .is_some_and(|pod| auto_remove(&pod))
        {
            return Ok(());
        }
        let records = self
            .container_manager
            .list_containers_in_pod(id)
            .await
            .map_err(Status::from)?;
        let finished = match records.as_slice() {
            [] => allow_empty,
            [container] => {
                container.status == ContainerStatus::Stopped
                    || (container.status == ContainerStatus::Unknown
                        && container.vm.as_ref().is_some_and(|vm| vm.logs_complete))
            }
            _ => false,
        };
        if finished {
            self.stop_sandbox(id).await?;
            self.pod_manager
                .remove_pod_locked(id)
                .await
                .map_err(Status::from)?;
        }
        Ok(())
    }
}
