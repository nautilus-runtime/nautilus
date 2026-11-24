//! Translate authenticated guest cgroup samples into kubelet's CRI statistics.
use super::NautilusRuntimeService;
use crate::core::state::{ContainerState, PodState};
use crate::proto::runtime::v1 as rt;
use crate::vm::workload::{Operation, Reply, Sample};
use std::io;
use tonic::Status;

pub(super) fn container_attributes(c: &ContainerState) -> rt::ContainerAttributes {
    rt::ContainerAttributes {
        id: c.id.clone(),
        metadata: Some(rt::ContainerMetadata {
            name: c.name.clone(),
            attempt: c.attempt,
        }),
        labels: c.labels.clone(),
        annotations: c.annotations.clone(),
    }
}

pub(super) fn pod_attributes(p: &PodState) -> rt::PodSandboxAttributes {
    rt::PodSandboxAttributes {
        id: p.id.clone(),
        metadata: Some(rt::PodSandboxMetadata {
            name: p.name.clone(),
            namespace: p.namespace.clone(),
            uid: p.uid.clone(),
            attempt: p.vm_attempt.as_ref().map_or(p.attempt, |a| a.attempt),
        }),
        labels: p.labels.clone(),
        annotations: p.annotations.clone(),
    }
}

fn readings(s: Sample) -> Result<(rt::CpuUsage, rt::MemoryUsage), &'static str> {
    if s.timestamp <= 0 || s.working_set_bytes > s.memory_usage_bytes {
        return Err("invalid guest cgroup sample");
    }
    let value = |n| rt::UInt64Value { value: n };
    Ok((
        rt::CpuUsage {
            timestamp: s.timestamp,
            usage_core_nano_seconds: Some(value(s.cpu_usage_nanos)),
            usage_nano_cores: s.usage_nano_cores.map(value),
            psi: None,
        },
        rt::MemoryUsage {
            timestamp: s.timestamp,
            working_set_bytes: Some(value(s.working_set_bytes)),
            usage_bytes: Some(value(s.memory_usage_bytes)),
            available_bytes: s.available_bytes.map(value),
            rss_bytes: Some(value(s.rss_bytes)),
            page_faults: Some(value(s.page_faults)),
            major_page_faults: Some(value(s.major_page_faults)),
            psi: None,
        },
    ))
}

fn container_stats(c: &ContainerState, s: Sample) -> Result<rt::ContainerStats, &'static str> {
    let (cpu, memory) = readings(s)?;
    Ok(rt::ContainerStats {
        attributes: Some(container_attributes(c)),
        cpu: Some(cpu),
        memory: Some(memory),
        ..Default::default()
    })
}

impl NautilusRuntimeService {
    async fn guest_sample(
        &self,
        pod: &str,
        id: &str,
        op: Operation<'_>,
    ) -> Result<(Sample, Reply), Status> {
        let channel = self
            .pod_manager
            .vm_store()
            .workload_channel(pod)
            .await
            .map_err(|e| Status::unavailable(format!("guest stats channel for Pod {pod}: {e}")))?;
        let mut reply = channel.call(id, op).await.map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => Status::not_found(e.to_string()),
            io::ErrorKind::AlreadyExists => Status::failed_precondition(e.to_string()),
            // Older workload agents reject unknown operations and continue serving other RPCs.
            io::ErrorKind::InvalidInput => Status::unimplemented(e.to_string()),
            io::ErrorKind::InvalidData => Status::data_loss(e.to_string()),
            _ => Status::unavailable(format!("guest cgroup stats for Pod {pod}: {e}")),
        })?;
        let sample = reply
            .stats
            .take()
            .ok_or_else(|| Status::data_loss("missing guest cgroup sample"))?;
        Ok((sample, reply))
    }

    pub(super) async fn guest_container_stats(
        &self,
        c: &ContainerState,
    ) -> Result<rt::ContainerStats, Status> {
        let (sample, _) = self
            .guest_sample(&c.pod_id, &c.id, Operation::Stats)
            .await?;
        container_stats(c, sample).map_err(Status::data_loss)
    }

    pub(super) async fn guest_pod_stats(
        &self,
        pod: &PodState,
    ) -> Result<rt::PodSandboxStats, Status> {
        let (sample, mut reply) = self.guest_sample(&pod.id, "", Operation::PodStats).await?;
        let (cpu, memory) = readings(sample).map_err(Status::data_loss)?;
        let mut containers = Vec::new();
        for c in self
            ._state_manager
            .list_containers_in_pod(&pod.id)
            .await
            .map_err(Status::from)?
        {
            if let Some(s) = reply.containers.remove(&c.id) {
                containers.push(container_stats(&c, s).map_err(Status::data_loss)?);
            }
        }
        Ok(rt::PodSandboxStats {
            attributes: Some(pod_attributes(pod)),
            linux: Some(rt::LinuxPodSandboxStats {
                cpu: Some(cpu),
                memory: Some(memory),
                containers,
                ..Default::default()
            }),
            windows: None,
        })
    }
}
