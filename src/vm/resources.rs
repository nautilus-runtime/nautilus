//! Boot sizing and systemd limits for a CRI Pod VM.
use crate::proto::runtime::v1::{LinuxContainerResources, LinuxPodSandboxConfig};
use std::io::{self, ErrorKind};
use std::path::{Component, Path};

const DEFAULT_MEMORY: u64 = 384 << 20;
const DEFAULT_PERIOD: u64 = 100_000;
const MIB: u64 = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmResources {
    pub vcpus: u8,
    pub guest_memory_bytes: u64,
    pub(crate) slice: Option<String>,
    weight: Option<u64>,
    cpu: Option<(u64, u64)>,
    memory_max: Option<u64>,
    oom_group: bool,
}

impl Default for VmResources {
    fn default() -> Self {
        Self {
            vcpus: 1,
            guest_memory_bytes: DEFAULT_MEMORY,
            slice: None,
            weight: None,
            cpu: None,
            memory_max: None,
            oom_group: false,
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, message)
}

fn unsupported(message: &str) -> io::Error {
    io::Error::new(ErrorKind::Unsupported, message)
}

fn validate(r: &LinuxContainerResources) -> io::Result<()> {
    if (1..1000).contains(&r.cpu_quota)
        || r.cpu_quota < -1
        || r.memory_limit_in_bytes < -1
        || (r.cpu_period != 0 && !(1_000..=1_000_000).contains(&r.cpu_period))
        || (r.cpu_shares != 0 && !(2..=262_144).contains(&r.cpu_shares))
    {
        return Err(invalid("invalid Pod memory, CPU quota, period or shares"));
    }
    if !r.cpuset_cpus.is_empty()
        || !r.cpuset_mems.is_empty()
        || !r.hugepage_limits.is_empty()
        || r.unified.iter().any(|(k,v)| k != "memory.oom.group" || v != "1")
        || r.oom_score_adj != 0
        || r.memory_swap_limit_in_bytes != 0
    {
        return Err(unsupported(
            "VM Pod cpusets, hugepages, unified controls, OOM adjustment and swap are unsupported",
        ));
    }
    Ok(())
}

// Accept a systemd slice name or its exact cgroupfs path. Moving the service
// into a similarly named slice would charge the wrong Kubernetes Pod.
pub(crate) fn parent_slice(parent: &str) -> io::Result<Option<String>> {
    if parent == "/" || parent == "-.slice" {
        return Ok(Some("-.slice".into()));
    }
    if parent.is_empty() {
        return Ok(None);
    }
    let path = Path::new(parent);
    let parts: Vec<_> = path
        .components()
        .filter_map(|part| match part {
            Component::RootDir => None,
            Component::Normal(name) => Some(name.to_str().unwrap_or("")),
            _ => Some(""),
        })
        .collect();
    let leaf = parts
        .last()
        .ok_or_else(|| invalid("empty Pod cgroup parent"))?;
    let stem = leaf
        .strip_suffix(".slice")
        .ok_or_else(|| unsupported("Pod VM requires a systemd slice cgroup parent"))?;
    if stem.is_empty()
        || *leaf == "-.slice"
        || leaf.len() > 255
        || !stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(invalid("invalid Pod slice name"));
    }
    let names: Vec<_> = stem.split('-').collect();
    if names.contains(&"") {
        return Err(invalid("invalid Pod slice hierarchy"));
    }
    if parts.len() > 1 || path.is_absolute() {
        let expected: Vec<_> = (1..=names.len())
            .map(|i| format!("{}.slice", names[..i].join("-")))
            .collect();
        if expected != parts {
            return Err(unsupported(
                "Pod cgroup path does not match its systemd slice hierarchy",
            ));
        }
    }
    Ok(Some((*leaf).to_owned()))
}

impl VmResources {
    pub fn from_cri(config: Option<&LinuxPodSandboxConfig>) -> io::Result<Self> {
        let Some(config) = config else {
            return Ok(Self::default());
        };
        let unset = LinuxContainerResources::default();
        let work = config.resources.as_ref().unwrap_or(&unset);
        let overhead = config.overhead.as_ref().unwrap_or(&unset);
        validate(work)?;
        validate(overhead)?;
        let mut plan = Self {
            slice: parent_slice(&config.cgroup_parent)?,
            oom_group: work.unified.contains_key("memory.oom.group") || overhead.unified.contains_key("memory.oom.group"),
            ..Self::default()
        };

        if work.memory_limit_in_bytes > 0 {
            let limit = work.memory_limit_in_bytes as u64;
            // Cloud Hypervisor RAM regions must be multiples of 2 MiB.
            let size = limit / (2 * MIB) * (2 * MIB);
            if size == 0 {
                return Err(invalid(
                    "Pod memory limit cannot hold a 2 MiB VM RAM region",
                ));
            }
            plan.guest_memory_bytes = size;
            let total = work
                .memory_limit_in_bytes
                .checked_add(overhead.memory_limit_in_bytes.max(0))
                .ok_or_else(|| invalid("Pod memory limit plus overhead overflows"))?;
            plan.memory_max = Some(total as u64);
        }
        if work.cpu_shares > 0 || overhead.cpu_shares > 0 {
            let shares = (work.cpu_shares + overhead.cpu_shares).clamp(2, 262_144) as u64;
            // Same conversion as Kubernetes v1.37.2's vendored
            // opencontainers/cgroups ConvertCPUSharesToCgroupV2Value.
            plan.weight = Some(match shares {
                2 => 1,
                1024 => 100,
                262_144 => 10_000,
                _ => {
                    let log = (shares as f64).log2();
                    10_f64
                        .powf((log * log + 125.0 * log) / 612.0 - 7.0 / 34.0)
                        .ceil() as u64
                }
            });
        }
        if work.cpu_quota > 0 {
            let period = if work.cpu_period == 0 {
                DEFAULT_PERIOD
            } else {
                work.cpu_period as u64
            };
            let quota = work.cpu_quota as u64;
            // The guest gets enough vCPUs to use its declared bandwidth.
            // systemd separately enforces the exact host-side bandwidth.
            plan.vcpus = u8::try_from(quota.div_ceil(period))
                .map_err(|_| invalid("Pod CPU quota exceeds maximum VM vCPU count"))?;
            let aux_period = if overhead.cpu_period == 0 {
                DEFAULT_PERIOD
            } else {
                overhead.cpu_period as u64
            };
            let scaled = overhead.cpu_quota.max(0) as u128 * period as u128;
            if !scaled.is_multiple_of(aux_period as u128) {
                return Err(unsupported(
                    "Pod CPU overhead is not representable in the workload period",
                ));
            }
            let extra = scaled / aux_period as u128;
            let total = u64::try_from(quota as u128 + extra)
                .map_err(|_| invalid("Pod CPU quota plus overhead overflows"))?;
            // systemd-run accepts percentages to two decimal places.
            let basis_points = total as u128 * 10_000;
            if !basis_points.is_multiple_of(period as u128)
                || basis_points / period as u128 > i32::MAX as u128
            {
                return Err(unsupported(
                    "Pod CPU quota cannot be represented by this systemd version",
                ));
            }
            plan.cpu = Some((total, period));
        }
        Ok(plan)
    }

    pub(crate) fn systemd_properties(&self) -> Vec<String> {
        let mut properties = Vec::new();
        if self.oom_group { properties.push("OOMPolicy=kill".into()); }
        if let Some(weight) = self.weight {
            properties.push(format!("CPUWeight={weight}"));
        }
        if let Some((quota, period)) = self.cpu {
            let points = quota as u128 * 10_000 / period as u128;
            properties.push(format!("CPUQuota={}.{:02}%", points / 100, points % 100));
            properties.push(format!("CPUQuotaPeriodSec={period}us"));
        }
        if let Some(memory) = self.memory_max {
            properties.push(format!("MemoryMax={memory}"));
        }
        properties
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_weights_and_memory_have_distinct_controls() {
        let work = LinuxContainerResources {
            cpu_quota: 125_000,
            cpu_period: 100_000,
            cpu_shares: 1024,
            memory_limit_in_bytes: 512 << 20,
            ..Default::default()
        };
        let overhead = LinuxContainerResources {
            cpu_quota: 5000,
            cpu_period: 50_000,
            memory_limit_in_bytes: 64 << 20,
            ..Default::default()
        };
        let pod = LinuxPodSandboxConfig {
            resources: Some(work),
            overhead: Some(overhead),
            cgroup_parent:
                "/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-poda12.slice".into(),
            ..Default::default()
        };
        let plan = VmResources::from_cri(Some(&pod)).unwrap();
        assert_eq!(
            plan.slice.as_deref(),
            Some("kubepods-burstable-poda12.slice")
        );
        assert_eq!((plan.vcpus, plan.guest_memory_bytes), (2, 512 << 20));
        assert_eq!(
            plan.systemd_properties(),
            [
                "CPUWeight=100",
                "CPUQuota=135.00%",
                "CPUQuotaPeriodSec=100000us",
                "MemoryMax=603979776"
            ]
        );
    }

    #[test]
    fn overhead_alone_does_not_create_a_workload_limit() {
        let pod = LinuxPodSandboxConfig {
            overhead: Some(LinuxContainerResources {
                memory_limit_in_bytes: 128 << 20,
                cpu_quota: 1000,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            VmResources::from_cri(Some(&pod)).unwrap(),
            VmResources::default()
        );
        assert!(parent_slice("/wrong.slice/kubepods-pod.slice").is_err());
        assert_eq!(parent_slice("/").unwrap().as_deref(), Some("-.slice"));
    }

    #[test]
    fn rejects_overhead_fractions_below_a_microsecond() {
        let budget = |period| {
            Some(LinuxContainerResources {
                cpu_quota: 1000,
                cpu_period: period,
                ..Default::default()
            })
        };
        let config = LinuxPodSandboxConfig {
            resources: budget(1000),
            overhead: budget(3000),
            ..Default::default()
        };
        assert_eq!(
            VmResources::from_cri(Some(&config)).unwrap_err().kind(),
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn unsupported_inputs_fail_before_any_allocation() {
        let r = LinuxContainerResources {
            cpuset_cpus: "1-2".into(),
            ..Default::default()
        };
        assert_eq!(validate(&r).unwrap_err().kind(), ErrorKind::Unsupported);
        assert!(validate(&LinuxContainerResources {
            cpu_period: 99,
            ..Default::default()
        })
        .is_err());
        assert!(parent_slice("/sys/fs/cgroup/not-a-slice").is_err());
    }
}
