//! Persist container ownership before staging and observe the guest's process state.
use super::{ContainerManager, ContainerState, ContainerStatus, Error, Result, WorkloadSpec};
use crate::runtime::vm_log::Target;
use crate::sandbox::{vm_backend::VmBackend, SandboxBackend};
use crate::vm::workload::{Operation, Reply};
use chrono::DateTime;
use futures::{stream, StreamExt};
use std::{io, path::Path, time::Duration};

impl ContainerManager {
    async fn save_guest(&self, state: &ContainerState) -> Result<()> {
        let mut containers = self.containers.write().await;
        self.state_manager.save_container_state(state).await?;
        containers.insert(state.id.clone(), state.clone());
        Ok(())
    }

    async fn forget_guest(&self, id: &str) -> Result<()> {
        let mut containers = self.containers.write().await;
        containers.remove(id);
        self.state_manager.delete_container_state(id).await?;
        Ok(())
    }

    pub(super) async fn create_guest(
        &self,
        mut state: ContainerState,
        spec: WorkloadSpec,
    ) -> Result<String> {
        let guard = self.vm_operations.lock(&state.id).await;
        state.status = ContainerStatus::Unknown;
        self.save_guest(&state).await?;
        let manager = self.clone();
        // Finish or roll back the intent even when the gRPC caller disconnects.
        let (reply, result) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _guard = guard;
            let backend = VmBackend::new(manager._config.clone(), &state.pod_id);
            let outcome = async {
                // Store the anonymous file identity before making the kubelet's log name visible.
                let claim = manager.log_target(&state).await?.claim()?;
                state.vm.as_mut().unwrap().cursor = claim.cursor();
                manager.save_guest(&state).await?;
                claim.publish()?;
                state.image_id = backend.prepare_image(&spec).await?;
                let created = backend.call(&state.id, Operation::Status).await?;
                observe(&mut state, created)?;
                state.vm.as_mut().expect("VM creation intent").preparing = false;
                manager.save_guest(&state).await?;
                Ok(state.id.clone())
            }
            .await;
            let cleanup = outcome.is_err() || reply.is_closed();
            if cleanup {
                if let Err(error) = manager.rollback_guest(&state).await {
                    tracing::warn!(container_id=%state.id,%error,"guest create retained for cleanup");
                }
            }
            if let Err(Ok(_)) = reply.send(outcome) {
                if !cleanup {
                    let _ = manager.rollback_guest(&state).await;
                }
            }
        });
        result
            .await
            .map_err(|_| Error::Runtime("VM container preparation interrupted".into()))?
    }

    async fn log_target(&self, state: &ContainerState) -> Result<Target> {
        let vm = state
            .vm
            .as_ref()
            .ok_or_else(|| Error::Runtime("missing VM container log record".into()))?;
        let pod = self
            .state_manager
            .get_pod_state(&state.pod_id)
            .await?
            .ok_or_else(|| Error::Pod("VM Pod state is missing".into()))?;
        Ok(Target::new(
            Path::new(&pod.log_directory),
            Path::new(&vm.relative_log),
            &self._config.storage.container_dir,
            &state.id,
        )?)
    }

    async fn sync_guest(
        &self,
        state: &mut ContainerState,
        backend: &VmBackend,
        drain: bool,
        mut changed: bool,
    ) -> Result<()> {
        let target = self.log_target(state).await?;
        loop {
            let vm = state.vm.as_mut().expect("validated VM log record");
            let id = &state.id;
            let (next, more) = target
                .sync_chunk(&vm.cursor, |offset, limit| async move {
                    let reply = backend
                        .call(
                            id,
                            Operation::ReadLog {
                                offset,
                                limit: limit as u32,
                            },
                        )
                        .await?;
                    let next = reply
                        .next_offset
                        .ok_or_else(|| Error::Runtime("guest omitted log offset".into()))?;
                    Ok((reply.stdout()?, next))
                })
                .await?;
            if vm.cursor != next {
                vm.cursor = next;
                changed = true;
            }
            if drain && !more && !vm.logs_complete {
                vm.logs_complete = true;
                changed = true;
            }
            if changed {
                self.save_guest(state).await?;
                changed = false;
            }
            if !drain || !more {
                return Ok(());
            }
        }
    }

    pub(crate) async fn reopen_guest_log(&self, id: &str) -> Result<()> {
        let _guard = self.vm_operations.lock(id).await;
        if let Some(mut state) = self.state_manager.get_container_state(id).await? {
            let target = self.log_target(&state).await?;
            let vm = state.vm.as_mut().unwrap();
            vm.cursor = target.reopen(&vm.cursor)?;
            self.save_guest(&state).await?;
        }
        Ok(())
    }

    pub(super) async fn guest_transition(&self, id: &str, op: Operation<'_>) -> Result<()> {
        let _guard = self.vm_operations.lock(id).await;
        let Some(mut state) = self.state_manager.get_container_state(id).await? else {
            return Err(Error::Container("VM container not found".into()));
        };
        if state.vm.as_ref().is_none_or(|v| v.preparing) {
            return Err(Error::Runtime(
                "VM container preparation is incomplete".into(),
            ));
        }
        let backend = VmBackend::new(self._config.clone(), &state.pod_id);
        let observing = matches!(&op, Operation::Status);
        let reply = match backend.call(id, op).await {
            Ok(reply) => reply,
            Err(e) => {
                if observing
                    && !matches!(
                        state.status,
                        ContainerStatus::Unknown | ContainerStatus::Stopped
                    )
                {
                    state.status = ContainerStatus::Unknown;
                    state.finish_reason = "ContainerStatusUnknown".into();
                    self.save_guest(&state).await?;
                }
                return Err(e);
            }
        };
        let changed = observe(&mut state, reply)?;
        if changed {
            self.save_guest(&state).await?;
        }
        let exited = state.status == ContainerStatus::Stopped;
        self.sync_guest(&mut state, &backend, exited, false).await
    }

    pub(crate) async fn observed_container(&self, id: &str) -> Result<Option<ContainerState>> {
        match self.get_container_state(id).await? {
            Some(state)
                if state
                    .vm
                    .as_ref()
                    .is_some_and(|v| !v.preparing && !v.logs_complete) =>
            {
                let _ = self.guest_transition(id, Operation::Status).await;
                self.state_manager.get_container_state(id).await
            }
            stored => Ok(stored),
        }
    }

    pub(crate) async fn observed_containers(&self) -> Result<Vec<ContainerState>> {
        use futures::TryStreamExt;
        let records = self.state_manager.list_containers().await?;
        stream::iter(records)
            .map(|state| async move {
                if state
                    .vm
                    .as_ref()
                    .is_some_and(|v| !v.preparing && !v.logs_complete)
                {
                    self.observed_container(&state.id).await
                } else {
                    Ok(Some(state))
                }
            })
            .buffered(8)
            .try_filter_map(|state| std::future::ready(Ok(state)))
            .try_collect()
            .await
    }

    async fn rollback_guest(&self, state: &ContainerState) -> Result<()> {
        let cursor = &state.vm.as_ref().unwrap().cursor;
        if cursor.is_bound() {
            let backend = VmBackend::new(self._config.clone(), &state.pod_id);
            backend.remove(&state.id).await?;
            self.log_target(state).await?.discard_if_owned(cursor)?;
        }
        self.forget_guest(&state.id).await
    }

    // The VMM is gone and the guest exit code is unavailable.
    async fn mark_guest_lost(&self, state: &mut ContainerState) -> Result<()> {
        if let Some(vm) = state.vm.as_mut() {
            if state.status != ContainerStatus::Stopped {
                state.status = ContainerStatus::Unknown;
                state.finish_reason = "ContainerStatusUnknown".into();
                state.exit_code = None;
            }
            vm.logs_complete = true;
            self.save_guest(state).await?;
        }
        Ok(())
    }

    /// Called only after the owner's VMM shutdown is confirmed.
    pub(crate) async fn guest_pod_lost(&self, pod: &str) -> Result<()> {
        for record in self.state_manager.list_containers_in_pod(pod).await? {
            let _guard = self.vm_operations.lock(&record.id).await;
            if let Some(mut state) = self.state_manager.get_container_state(&record.id).await? {
                self.mark_guest_lost(&mut state).await?;
            }
        }
        Ok(())
    }

    async fn remove_guest_locked(&self, state: &ContainerState) -> Result<()> {
        let backend = VmBackend::new(self._config.clone(), &state.pod_id);
        backend.remove(&state.id).await?;
        self.forget_guest(&state.id).await
    }

    pub(super) async fn remove_guest(&self, id: &str) -> Result<()> {
        let _guard = self.vm_operations.lock(id).await;
        if let Some(mut state) = self.state_manager.get_container_state(id).await? {
            if state.vm.as_ref().is_some_and(|v| v.preparing) {
                return self.rollback_guest(&state).await;
            }
            if state.vm.as_ref().is_some_and(|v| !v.logs_complete) {
                let backend = VmBackend::new(self._config.clone(), &state.pod_id);
                if crate::vm::owner::PodVmStore::new(self._config.runtime.state_dir.join("vms"))
                    .status(&state.pod_id)
                    .await?
                    .is_some()
                {
                    let reply = backend.call(id, Operation::Stop { timeout_ms: 0 }).await?;
                    observe(&mut state, reply)?;
                    self.sync_guest(&mut state, &backend, true, true).await?;
                } else {
                    self.mark_guest_lost(&mut state).await?;
                }
            }
            self.remove_guest_locked(&state).await?;
        }
        Ok(())
    }

    pub(crate) async fn recover_guest_containers(&self) -> Result<()> {
        let records = self.state_manager.list_containers().await?;
        for state in records
            .iter()
            .filter(|s| s.vm.as_ref().is_some_and(|v| v.preparing))
        {
            if let Err(e) = self.remove_guest(&state.id).await {
                tracing::warn!(container_id=%state.id,%e,"incomplete container retained for cleanup");
            }
        }
        if records.iter().any(|s| s.vm.is_some()) || self._config.runtime.sandboxes.vm.is_some() {
            let manager = self.clone();
            tokio::spawn(async move {
                loop {
                    if let Ok(records) = manager.state_manager.list_containers().await {
                        stream::iter(records).filter(|s| std::future::ready(
                            s.vm.as_ref().is_some_and(|v| !v.preparing && !v.logs_complete)
                        )).for_each_concurrent(8, |state| {
                            let manager = &manager;
                            async move {
                                if let Err(e) = manager.guest_transition(&state.id, Operation::Status).await {
                                    tracing::debug!(container_id=%state.id,%e,"guest observation unavailable");
                                }
                            }
                        }).await;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            });
        }
        Ok(())
    }
}

fn observe(state: &mut ContainerState, reply: Reply) -> Result<bool> {
    let invalid = || {
        Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "inconsistent guest container status",
        ))
    };
    let timestamp = |n: Option<i64>| n.filter(|v| *v > 0).map(DateTime::from_timestamp_nanos);
    let started = timestamp(reply.started_at);
    let finished = timestamp(reply.finished_at);
    let status = match reply.state.as_deref() {
        Some("created") if started.is_none() && finished.is_none() => ContainerStatus::Created,
        Some("running") if started.is_some() && finished.is_none() => ContainerStatus::Running,
        Some("exited") if finished.is_some() && reply.exit_code.is_some() => {
            ContainerStatus::Stopped
        }
        _ => return Err(invalid()),
    };
    let created = timestamp(reply.created_at).ok_or_else(invalid)?;
    let changed = state.status != status
        || state.created_at != created
        || state.started_at != started
        || state.finished_at != finished
        || state.exit_code != reply.exit_code
        || state.pid.is_some();
    state.status = status;
    state.finish_reason.clear();
    state.created_at = created;
    state.started_at = started;
    state.finished_at = finished;
    state.exit_code = reply.exit_code;
    state.pid = None;
    Ok(changed)
}
