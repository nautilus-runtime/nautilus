//! Collect local and guest workload samples for kubelet's CRI statistics.
use super::NautilusRuntimeService;
use crate::core::state::{ContainerState, ContainerStatus, PodState};
use crate::proto::runtime::v1 as rt;
use crate::sandbox::runc_backend::RuncBackend;
use crate::vm::workload::{Operation, Reply, Sample};
use pathrs::{flags::OpenFlags, Root};
use std::collections::HashMap;
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

pub(super) fn readings(s: &Sample) -> Result<(rt::CpuUsage, rt::MemoryUsage), &'static str> {
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
    let (cpu, memory) = readings(&s)?;
    Ok(rt::ContainerStats {
        attributes: Some(container_attributes(c)),
        cpu: Some(cpu),
        memory: Some(memory),
        ..Default::default()
    })
}

pub(super) fn stat_value(stat: &str, key: &str) -> Option<u64> {
    for line in stat.lines() {
        let mut fields = line.split_ascii_whitespace();
        if fields.next() == Some(key) {
            return fields.next()?.parse().ok();
        }
    }
    None
}

pub(super) fn pressure(stat: &str) -> Option<rt::PsiStats> {
    let parse = |class| {
        let line = stat.lines().find_map(|line| line.strip_prefix(class))?;
        let get = |key| {
            line.split_ascii_whitespace()
                .find_map(|part| part.strip_prefix(key))
        };
        let percent = |key| {
            get(key)?
                .parse::<f64>()
                .ok()
                .filter(|value| (0.0..=100.0).contains(value))
        };
        Some(rt::PsiData {
            total: get("total=")?.parse::<u64>().ok()?.checked_mul(1000)?,
            avg10: percent("avg10=")?,
            avg60: percent("avg60=")?,
            avg300: percent("avg300=")?,
        })
    };
    let some = parse("some ");
    let full = parse("full ");
    (some.is_some() || full.is_some()).then_some(rt::PsiStats { full, some })
}

pub(super) fn read_cgroup_file(group: &Root, name: &str) -> Option<String> {
    let mut file = group.open_subpath(name, OpenFlags::O_RDONLY).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
}

fn cgroup_usage(group: &Root, timestamp: i64) -> rt::ContainerStats {
    let read = |name| read_cgroup_file(group, name);
    let number = |name| read(name)?.trim().parse::<u64>().ok();
    let psi = |name| read(name).and_then(|stat| pressure(&stat));
    let value = |value| rt::UInt64Value { value };
    let cpu_total = read("cpu.stat")
        .and_then(|stat| stat_value(&stat, "usage_usec"))
        .and_then(|micros| micros.checked_mul(1000));
    let cpu_pressure = psi("cpu.pressure");
    let cpu = (cpu_total.is_some() || cpu_pressure.is_some()).then_some(rt::CpuUsage {
        timestamp,
        usage_core_nano_seconds: cpu_total.map(value),
        usage_nano_cores: None,
        psi: cpu_pressure,
    });

    let memory_stat = read("memory.stat");
    let mem = |key| stat_value(memory_stat.as_deref()?, key);
    let usage = number("memory.current");
    let working_set = usage
        .zip(mem("inactive_file"))
        .map(|(usage, inactive)| usage.saturating_sub(inactive));
    let major_faults = mem("pgmajfault");
    let rss = mem("anon")
        .zip(mem("swapcached"))
        .and_then(|(anon, swapcached)| anon.checked_add(swapcached));
    let memory_pressure = psi("memory.pressure");
    let memory =
        (usage.is_some() || rss.is_some() || major_faults.is_some() || memory_pressure.is_some())
            .then(|| rt::MemoryUsage {
                timestamp,
                usage_bytes: usage.map(value),
                working_set_bytes: working_set.map(value),
                available_bytes: working_set
                    .zip(number("memory.max"))
                    .map(|(working_set, limit)| value(limit.saturating_sub(working_set))),
                rss_bytes: rss.map(value),
                // Linux's pgfault includes major faults, while CRI asks for minor faults.
                page_faults: mem("pgfault")
                    .zip(major_faults)
                    .and_then(|(all, major)| all.checked_sub(major))
                    .map(value),
                major_page_faults: major_faults.map(value),
                psi: memory_pressure,
            });
    let swap = number("memory.swap.current").map(|usage| rt::SwapUsage {
        timestamp,
        swap_usage_bytes: Some(value(usage)),
        swap_available_bytes: number("memory.swap.max")
            .map(|limit| value(limit.saturating_sub(usage))),
    });
    let io = psi("io.pressure").map(|psi| rt::IoUsage {
        timestamp,
        psi: Some(psi),
    });
    rt::ContainerStats {
        cpu,
        memory,
        swap,
        io,
        ..Default::default()
    }
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
    pub(super) async fn read_native_cgroup<T: Send + 'static>(
        &self,
        c: &ContainerState,
        sample: impl FnOnce(&Root, &Path, &Path) -> T + Send + 'static,
    ) -> Result<Option<T>, Status> {
        if c.sandbox_kind != "runc" || self._config.runtime.runc.driver == "exec" {
            return Ok(None);
        }
        let pid = c
            .pid
            .filter(|&pid| {
                pid > 0 && matches!(c.status, ContainerStatus::Running | ContainerStatus::Paused)
            })
            .ok_or_else(|| Status::failed_precondition("container is not running"))?;
        let backend = RuncBackend::new(self._config.clone());
        let id = c.id.clone();
        tokio::task::spawn_blocking(move || {
            if !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file() {
                return Ok(None);
            }
            let path = backend
                .workload_cgroup_v2(&id, pid)
                .ok_or_else(|| Status::unavailable("owned container cgroup is not readable"))?;
            let group = Root::open(&path)
                .map_err(|_| Status::unavailable("owned container cgroup is not readable"))?;
            Ok::<_, Status>(Some(sample(&group, &path, &backend.bundle_dir(&id))))
        })
        .await
        .map_err(|e| Status::internal(format!("container stats worker: {e}")))?
    }

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
        let cgroup = self
            .read_native_cgroup(c, move |group, _, _| cgroup_usage(group, timestamp))
            .await?;
        let mut stats = match cgroup {
            // Init-only CPU may be lower than the previous cgroup sample.
            Some(stats) => stats,
            None => {
                let (cpu_ns, rss_bytes) = process_usage(pid).await?;
                rt::ContainerStats {
                    cpu: Some(rt::CpuUsage {
                        timestamp,
                        usage_core_nano_seconds: Some(rt::UInt64Value { value: cpu_ns }),
                        ..Default::default()
                    }),
                    memory: Some(rt::MemoryUsage {
                        timestamp,
                        working_set_bytes: Some(rt::UInt64Value { value: rss_bytes }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }
            }
        };
        stats.attributes = Some(container_attributes(c));
        Ok(stats)
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

    pub(super) async fn guest_pod_samples(
        &self,
        pod: &PodState,
    ) -> Result<(Sample, HashMap<String, Sample>), Status> {
        let (sample, reply) = self.guest_sample(&pod.id, "", Operation::PodStats).await?;
        Ok((sample, reply.containers))
    }

    pub(super) async fn guest_pod_stats(
        &self,
        pod: &PodState,
    ) -> Result<rt::PodSandboxStats, Status> {
        let (sample, mut samples) = self.guest_pod_samples(pod).await?;
        let (cpu, memory) = readings(&sample).map_err(Status::data_loss)?;
        let mut containers = Vec::new();
        for c in self
            ._state_manager
            .list_containers_in_pod(&pod.id)
            .await
            .map_err(Status::from)?
        {
            if let Some(s) = samples.remove(&c.id) {
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
    fn unified_cgroup_cpu_memory_and_swap_usage() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cpu.stat"),
            "usage_usec 134\nuser_usec 64\n",
        )
        .unwrap();
        fs::write(dir.path().join("memory.current"), "20480\n").unwrap();
        fs::write(
            dir.path().join("memory.stat"),
            "anon 8192\nswapcached 1024\ninactive_file 4096\npgfault 11\npgmajfault 2\n",
        )
        .unwrap();
        fs::write(dir.path().join("memory.max"), "32768\n").unwrap();
        fs::write(dir.path().join("memory.swap.current"), "4096\n").unwrap();
        fs::write(dir.path().join("memory.swap.max"), "8192\n").unwrap();

        let group = Root::open(dir.path()).unwrap();
        let stats = cgroup_usage(&group, 100);
        assert_eq!(
            stats.cpu.unwrap().usage_core_nano_seconds.unwrap().value,
            134_000
        );
        let memory = stats.memory.unwrap();
        assert_eq!(memory.timestamp, 100);
        assert_eq!(memory.usage_bytes.unwrap().value, 20480);
        assert_eq!(memory.working_set_bytes.unwrap().value, 16384);
        assert_eq!(memory.available_bytes.unwrap().value, 16384);
        assert_eq!(memory.rss_bytes.unwrap().value, 9216);
        assert_eq!(memory.page_faults.unwrap().value, 9);
        assert_eq!(memory.major_page_faults.unwrap().value, 2);
        let swap = stats.swap.unwrap();
        assert_eq!(swap.swap_usage_bytes.unwrap().value, 4096);
        assert_eq!(swap.swap_available_bytes.unwrap().value, 4096);

        fs::write(dir.path().join("memory.current"), "1024\n").unwrap();
        assert_eq!(
            cgroup_usage(&group, 200)
                .memory
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
        fs::write(dir.path().join("memory.swap.current"), "0\n").unwrap();
        fs::write(dir.path().join("memory.swap.max"), "max\n").unwrap();
        let stats = cgroup_usage(&group, 1);
        assert_eq!(stats.cpu.unwrap().usage_core_nano_seconds.unwrap().value, 0);
        let memory = stats.memory.unwrap();
        assert_eq!(memory.usage_bytes.unwrap().value, 20480);
        assert!(memory.working_set_bytes.is_none());
        assert!(memory.page_faults.is_none());
        let swap = stats.swap.unwrap();
        assert_eq!(swap.swap_usage_bytes.unwrap().value, 0);
        assert!(swap.swap_available_bytes.is_none());

        fs::write(
            dir.path().join("cpu.stat"),
            "usage_usec 18446744073709551615\n",
        )
        .unwrap();
        fs::write(dir.path().join("memory.stat"), "inactive_file 0\n").unwrap();
        let stats = cgroup_usage(&group, 1);
        assert!(stats.cpu.is_none());
        assert_eq!(
            stats.memory.unwrap().working_set_bytes.unwrap().value,
            20480
        );
    }

    #[test]
    fn unified_cgroup_pressure_is_independent_and_in_nanoseconds() {
        let dir = tempfile::tempdir().unwrap();
        let group = Root::open(dir.path()).unwrap();
        fs::write(
            dir.path().join("cpu.pressure"),
            "some avg10=0.00 avg60=0.25 avg300=4.50 total=178\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("memory.pressure"),
            "some avg10=NaN avg60=0.25 avg300=4.50 total=178\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("io.pressure"),
            "full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        )
        .unwrap();
        let stats = cgroup_usage(&group, 1);
        let cpu = stats.cpu.unwrap();
        assert!(cpu.usage_core_nano_seconds.is_none());
        let cpu_psi = cpu.psi.unwrap();
        assert!(cpu_psi.full.is_none());
        let some = cpu_psi.some.unwrap();
        assert_eq!(some.total, 178_000);
        assert_eq!(some.avg60, 0.25);
        assert!(stats.memory.is_none());
        assert_eq!(stats.io.unwrap().psi.unwrap().full.unwrap().total, 0);
    }
}
