//! Collect local and guest workload samples for kubelet's CRI statistics.
use super::NautilusRuntimeService;
use crate::core::state::{ContainerState, ContainerStatus, PodState};
use crate::proto::runtime::v1 as rt;
use crate::sandbox::runc_backend::RuncBackend;
use crate::vm::workload::{Operation, Reply, Sample};
use pathrs::{flags::OpenFlags, Root};
use std::io::{self, Read};
use std::path::Path;
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

fn stat_value(stat: &str, key: &str) -> Option<u64> {
    for line in stat.lines() {
        let mut fields = line.split_ascii_whitespace();
        if fields.next() == Some(key) {
            return fields.next()?.parse().ok();
        }
    }
    None
}

fn cgroup_usage(group: &Root, timestamp: i64) -> (Option<u64>, Option<rt::MemoryUsage>) {
    let read = |name: &str| {
        let mut file = group.open_subpath(name, OpenFlags::O_RDONLY).ok()?;
        let mut text = String::new();
        file.read_to_string(&mut text).ok()?;
        Some(text)
    };
    let cpu = read("cpu.stat")
        .and_then(|stat| stat_value(&stat, "usage_usec"))
        .and_then(|micros| micros.checked_mul(1000));
    let memory = read("memory.current")
        .and_then(|current| current.trim().parse::<u64>().ok())
        .map(|usage| rt::MemoryUsage {
            timestamp,
            usage_bytes: Some(rt::UInt64Value { value: usage }),
            working_set_bytes: read("memory.stat")
                .and_then(|stat| stat_value(&stat, "inactive_file"))
                .map(|inactive_file| rt::UInt64Value {
                    value: usage.saturating_sub(inactive_file),
                }),
            ..Default::default()
        });
    (cpu, memory)
}

async fn process_usage(pid: u32) -> Result<(u64, u64), Status> {
    let stat = tokio::fs::read_to_string(format!("/proc/{pid}/stat"))
        .await
        .map_err(|_| Status::internal("container process not readable (exited?)"))?;
    // Fields 14 and 15 are CPU ticks. The comm field can contain spaces and parentheses.
    let tail = stat.rsplit(')').next().unwrap_or_default();
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let ticks_per_sec = 100f64; // sysconf(_SC_CLK_TCK), Linux default
    let utime: f64 = fields.get(11).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let stime: f64 = fields.get(12).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let rss_pages: u64 = fields.get(21).and_then(|v| v.parse().ok()).unwrap_or(0);
    Ok((
        ((utime + stime) / ticks_per_sec * 1e9) as u64,
        rss_pages * 4096,
    ))
}

impl NautilusRuntimeService {
    pub(super) async fn local_container_stats(
        &self,
        c: &ContainerState,
    ) -> Result<rt::ContainerStats, Status> {
        if !matches!(c.status, ContainerStatus::Running | ContainerStatus::Paused) {
            return Err(Status::failed_precondition("container is not running"));
        }
        let pid = c
            .pid
            .filter(|&pid| pid > 0)
            .ok_or_else(|| Status::failed_precondition("container is not running"))?;
        let timestamp = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let cgroup = if c.sandbox_kind == "runc" && self._config.runtime.runc.driver != "exec" {
            let backend = RuncBackend::new(self._config.clone());
            let id = c.id.clone();
            tokio::task::spawn_blocking(move || {
                if !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file() {
                    return Ok(None);
                }
                let group = backend
                    .workload_cgroup_v2(&id, pid)
                    .and_then(|path| Root::open(path).ok())
                    .ok_or_else(|| Status::unavailable("owned container cgroup is not readable"))?;
                Ok::<_, Status>(Some(cgroup_usage(&group, timestamp)))
            })
            .await
            .map_err(|e| Status::internal(format!("container stats worker: {e}")))??
        } else {
            None
        };
        let (cpu, memory) = match cgroup {
            // Init-only CPU may be lower than the previous cgroup sample.
            Some((cpu, memory)) => (cpu, memory),
            None => {
                let (cpu_ns, rss_bytes) = process_usage(pid).await?;
                let memory = rt::MemoryUsage {
                    timestamp,
                    working_set_bytes: Some(rt::UInt64Value { value: rss_bytes }),
                    ..Default::default()
                };
                (Some(cpu_ns), Some(memory))
            }
        };
        Ok(rt::ContainerStats {
            attributes: Some(container_attributes(c)),
            cpu: cpu.map(|value| rt::CpuUsage {
                timestamp,
                usage_core_nano_seconds: Some(rt::UInt64Value { value }),
                ..Default::default()
            }),
            memory,
            ..Default::default()
        })
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn unified_cgroup_cpu_and_memory_usage() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cpu.stat"),
            "usage_usec 134\nuser_usec 64\n",
        )
        .unwrap();
        fs::write(dir.path().join("memory.current"), "20480\n").unwrap();
        fs::write(
            dir.path().join("memory.stat"),
            "anon 8192\ninactive_file 4096\n",
        )
        .unwrap();

        let group = Root::open(dir.path()).unwrap();
        let (cpu, memory) = cgroup_usage(&group, 100);
        assert_eq!(cpu, Some(134_000));
        let memory = memory.unwrap();
        assert_eq!(memory.timestamp, 100);
        assert_eq!(memory.usage_bytes.unwrap().value, 20480);
        assert_eq!(memory.working_set_bytes.unwrap().value, 16384);

        fs::write(dir.path().join("memory.current"), "1024\n").unwrap();
        assert_eq!(
            cgroup_usage(&group, 200)
                .1
                .unwrap()
                .working_set_bytes
                .unwrap()
                .value,
            0
        );
    }

    #[test]
    fn unified_cgroup_counters_can_be_sampled_independently() {
        let dir = tempfile::tempdir().unwrap();
        let group = Root::open(dir.path()).unwrap();
        fs::write(dir.path().join("cpu.stat"), "usage_usec 0\n").unwrap();
        fs::write(dir.path().join("memory.current"), "20480\n").unwrap();
        let (cpu, memory) = cgroup_usage(&group, 1);
        assert_eq!(cpu, Some(0));
        let memory = memory.unwrap();
        assert_eq!(memory.usage_bytes.unwrap().value, 20480);
        assert!(memory.working_set_bytes.is_none());

        fs::write(
            dir.path().join("cpu.stat"),
            "usage_usec 18446744073709551615\n",
        )
        .unwrap();
        fs::write(dir.path().join("memory.stat"), "inactive_file 0\n").unwrap();
        let (cpu, memory) = cgroup_usage(&group, 1);
        assert!(cpu.is_none());
        assert_eq!(memory.unwrap().working_set_bytes.unwrap().value, 20480);
    }
}
