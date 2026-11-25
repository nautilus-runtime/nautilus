use super::{stats, NautilusRuntimeService};
use crate::core::state::{ContainerState, PodState, PodStatus};
use crate::proto::runtime::v1::{
    ContainerMetrics, CpuUsage, MemoryUsage, Metric, MetricDescriptor, MetricType,
    PodSandboxMetrics, UInt64Value,
};
use futures::StreamExt;
use pathrs::{error::ErrorKind, flags::OpenFlags, Root};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tonic::Status;

const LABELS: [&str; 5] = ["id", "namespace", "pod", "container", "image"];
const SECOND_NS: u64 = 1_000_000_000;
const SECOND_US: u64 = 1_000_000;

#[derive(Clone, Copy)]
enum Source {
    Number(&'static str),
    Field(&'static str, &'static str),
    Seconds(&'static str, &'static str),
    Pressure(&'static str, bool),
    CpuMax(usize),
    CpuShares,
    BlockBytes,
    BlockIo(&'static str),
    Network(usize),
    Processes,
    Fds,
    Sockets,
    ThreadLimit,
    MemoryLimit(&'static str),
    Filesystem(FsValue),
    WorkingSet,
    Faults,
    LastSeen,
    Started,
}

#[derive(Clone, Copy)]
enum FsValue {
    Inodes,
    InodesFree,
    Limit,
    Usage,
}

struct Family {
    name: &'static str,
    help: &'static str,
    kind: MetricType,
    extra: &'static [&'static str],
    source: Source,
}

impl Family {
    const fn new(name: &'static str, help: &'static str, kind: MetricType, source: Source) -> Self {
        Self {
            name,
            help,
            kind,
            extra: &[],
            source,
        }
    }

    const fn labels(mut self, extra: &'static [&'static str]) -> Self {
        self.extra = extra;
        self
    }

    fn metric(&self, labels: &[String], extra: &[&str], value: u64) -> Metric {
        debug_assert_eq!(extra.len(), self.extra.len());
        Metric {
            name: self.name.into(),
            timestamp: 0,
            metric_type: self.kind as i32,
            label_values: labels
                .iter()
                .cloned()
                .chain(extra.iter().map(|s| (*s).into()))
                .collect(),
            value: Some(UInt64Value { value }),
        }
    }
}

use MetricType::{Counter, Gauge};
use Source::{
    BlockBytes, BlockIo, CpuMax, CpuShares, Faults, Fds, Field, Filesystem, LastSeen, MemoryLimit,
    Network, Number, Pressure, Processes, Seconds, Sockets, Started, ThreadLimit, WorkingSet,
};

const CPU_USAGE: Family = Family::new(
    "container_cpu_usage_seconds_total",
    "Cumulative CPU time consumed in seconds.",
    Counter,
    Seconds("cpu.stat", "usage_usec"),
)
.labels(&["cpu"]);
const MEMORY_WORKING_SET: Family = Family::new(
    "container_memory_working_set_bytes",
    "Current working set in bytes.",
    Gauge,
    WorkingSet,
);
const MEMORY_USAGE: Family = Family::new(
    "container_memory_usage_bytes",
    "Current memory usage in bytes.",
    Gauge,
    Number("memory.current"),
);

const FAMILIES: &[Family] = &[
    CPU_USAGE,
    Family::new(
        "container_cpu_user_seconds_total",
        "Cumulative user CPU time in seconds.",
        Counter,
        Seconds("cpu.stat", "user_usec"),
    ),
    Family::new(
        "container_cpu_system_seconds_total",
        "Cumulative system CPU time in seconds.",
        Counter,
        Seconds("cpu.stat", "system_usec"),
    ),
    Family::new(
        "container_cpu_cfs_periods_total",
        "Number of elapsed enforcement periods.",
        Counter,
        Field("cpu.stat", "nr_periods"),
    ),
    Family::new(
        "container_cpu_cfs_throttled_periods_total",
        "Number of throttled periods.",
        Counter,
        Field("cpu.stat", "nr_throttled"),
    ),
    Family::new(
        "container_cpu_cfs_throttled_seconds_total",
        "Cumulative time throttled in seconds.",
        Counter,
        Seconds("cpu.stat", "throttled_usec"),
    ),
    Family::new(
        "container_spec_cpu_period",
        "CPU enforcement period in microseconds.",
        Gauge,
        CpuMax(1),
    ),
    Family::new(
        "container_spec_cpu_quota",
        "CPU quota in microseconds per enforcement period.",
        Gauge,
        CpuMax(0),
    ),
    Family::new(
        "container_spec_cpu_shares",
        "CPU shares equivalent to the cgroup weight.",
        Gauge,
        CpuShares,
    ),
    MEMORY_WORKING_SET,
    MEMORY_USAGE,
    Family::new(
        "container_memory_cache",
        "Bytes of filesystem cache memory.",
        Gauge,
        Field("memory.stat", "file"),
    ),
    Family::new(
        "container_memory_rss",
        "Bytes of resident anonymous memory.",
        Gauge,
        Field("memory.stat", "anon"),
    ),
    Family::new(
        "container_memory_kernel_usage",
        "Bytes of kernel memory allocated.",
        Gauge,
        Field("memory.stat", "kernel"),
    ),
    Family::new(
        "container_memory_mapped_file",
        "Bytes of mapped file memory.",
        Gauge,
        Field("memory.stat", "file_mapped"),
    ),
    Family::new(
        "container_memory_max_usage_bytes",
        "Maximum memory usage recorded in bytes.",
        Gauge,
        Number("memory.peak"),
    ),
    Family::new(
        "container_memory_swap",
        "Current swap usage in bytes.",
        Gauge,
        Number("memory.swap.current"),
    ),
    Family::new(
        "container_memory_failcnt",
        "Number of memory maximum limit hits.",
        Counter,
        Field("memory.events", "max"),
    ),
    Family::new(
        "container_memory_failures_total",
        "Cumulative page faults by kind and scope.",
        Counter,
        Faults,
    )
    .labels(&["failure_type", "scope"]),
    Family::new(
        "container_oom_events_total",
        "Number of out-of-memory events.",
        Counter,
        Field("memory.events", "oom"),
    ),
    Family::new(
        "container_spec_memory_limit_bytes",
        "Per-cgroup memory limit in bytes, or zero when unlimited.",
        Gauge,
        MemoryLimit("memory.max"),
    ),
    Family::new(
        "container_spec_memory_swap_limit_bytes",
        "Per-cgroup swap limit in bytes, or zero when unlimited.",
        Gauge,
        MemoryLimit("memory.swap.max"),
    ),
    Family::new(
        "container_pressure_cpu_stalled_seconds_total",
        "CPU full stall time in seconds.",
        Counter,
        Pressure("cpu.pressure", true),
    ),
    Family::new(
        "container_pressure_cpu_waiting_seconds_total",
        "CPU stall time for one or more tasks in seconds.",
        Counter,
        Pressure("cpu.pressure", false),
    ),
    Family::new(
        "container_pressure_io_stalled_seconds_total",
        "I/O full stall time in seconds.",
        Counter,
        Pressure("io.pressure", true),
    ),
    Family::new(
        "container_pressure_io_waiting_seconds_total",
        "I/O stall time for one or more tasks in seconds.",
        Counter,
        Pressure("io.pressure", false),
    ),
    Family::new(
        "container_pressure_memory_stalled_seconds_total",
        "Memory full stall time in seconds.",
        Counter,
        Pressure("memory.pressure", true),
    ),
    Family::new(
        "container_pressure_memory_waiting_seconds_total",
        "Memory stall time for one or more tasks in seconds.",
        Counter,
        Pressure("memory.pressure", false),
    ),
    Family::new(
        "container_threads",
        "Current number of cgroup tasks.",
        Gauge,
        Number("pids.current"),
    ),
    Family::new(
        "container_threads_max",
        "Per-cgroup task limit, or zero when unlimited.",
        Gauge,
        ThreadLimit,
    ),
    Family::new(
        "container_processes",
        "Number of processes in the workload cgroup.",
        Gauge,
        Processes,
    ),
    Family::new(
        "container_file_descriptors",
        "Open file descriptors of processes in the workload cgroup.",
        Gauge,
        Fds,
    ),
    Family::new(
        "container_sockets",
        "Open socket descriptors of processes in the workload cgroup.",
        Gauge,
        Sockets,
    ),
    Family::new(
        "container_last_seen",
        "Unix time the workload was sampled in seconds.",
        Gauge,
        LastSeen,
    ),
    Family::new(
        "container_start_time_seconds",
        "Unix time the workload started in seconds.",
        Gauge,
        Started,
    ),
    Family::new(
        "container_blkio_device_usage_total",
        "Cumulative block device bytes by operation.",
        Counter,
        BlockBytes,
    )
    .labels(&["device", "major", "minor", "operation"]),
    Family::new(
        "container_fs_reads_bytes_total",
        "Cumulative bytes read from the block device.",
        Counter,
        BlockIo("rbytes"),
    )
    .labels(&["device"]),
    Family::new(
        "container_fs_reads_total",
        "Cumulative block device reads completed.",
        Counter,
        BlockIo("rios"),
    )
    .labels(&["device"]),
    Family::new(
        "container_fs_writes_bytes_total",
        "Cumulative bytes written to the block device.",
        Counter,
        BlockIo("wbytes"),
    )
    .labels(&["device"]),
    Family::new(
        "container_fs_writes_total",
        "Cumulative block device writes completed.",
        Counter,
        BlockIo("wios"),
    )
    .labels(&["device"]),
    Family::new(
        "container_fs_inodes_total",
        "Total inodes on the workload's backing filesystem.",
        Gauge,
        Filesystem(FsValue::Inodes),
    )
    .labels(&["device"]),
    Family::new(
        "container_fs_inodes_free",
        "Available inodes on the workload's backing filesystem.",
        Gauge,
        Filesystem(FsValue::InodesFree),
    )
    .labels(&["device"]),
    Family::new(
        "container_fs_limit_bytes",
        "Capacity in bytes of the workload's backing filesystem.",
        Gauge,
        Filesystem(FsValue::Limit),
    )
    .labels(&["device"]),
    Family::new(
        "container_fs_usage_bytes",
        "Bytes occupied by the workload's writable snapshot.",
        Gauge,
        Filesystem(FsValue::Usage),
    )
    .labels(&["device"]),
    Family::new(
        "container_network_receive_bytes_total",
        "Cumulative bytes received by the pod interface.",
        Counter,
        Network(0),
    )
    .labels(&["interface"]),
    Family::new(
        "container_network_receive_packets_total",
        "Cumulative packets received by the pod interface.",
        Counter,
        Network(1),
    )
    .labels(&["interface"]),
    Family::new(
        "container_network_receive_errors_total",
        "Cumulative receive errors on the pod interface.",
        Counter,
        Network(2),
    )
    .labels(&["interface"]),
    Family::new(
        "container_network_receive_packets_dropped_total",
        "Cumulative receive packet drops on the pod interface.",
        Counter,
        Network(3),
    )
    .labels(&["interface"]),
    Family::new(
        "container_network_transmit_bytes_total",
        "Cumulative bytes transmitted by the pod interface.",
        Counter,
        Network(8),
    )
    .labels(&["interface"]),
    Family::new(
        "container_network_transmit_packets_total",
        "Cumulative packets transmitted by the pod interface.",
        Counter,
        Network(9),
    )
    .labels(&["interface"]),
    Family::new(
        "container_network_transmit_errors_total",
        "Cumulative transmit errors on the pod interface.",
        Counter,
        Network(10),
    )
    .labels(&["interface"]),
    Family::new(
        "container_network_transmit_packets_dropped_total",
        "Cumulative transmit packet drops on the pod interface.",
        Counter,
        Network(11),
    )
    .labels(&["interface"]),
];

pub(super) fn descriptors() -> Vec<MetricDescriptor> {
    FAMILIES
        .iter()
        .map(|family| MetricDescriptor {
            name: family.name.into(),
            help: family.help.into(),
            label_keys: LABELS
                .into_iter()
                .chain(family.extra.iter().copied())
                .map(String::from)
                .collect(),
        })
        .collect()
}

fn labels(pod: &PodState, container: Option<&ContainerState>) -> Vec<String> {
    vec![
        container.map_or_else(|| pod.id.clone(), |c| c.id.clone()),
        pod.namespace.clone(),
        pod.name.clone(),
        container.map_or_else(|| "POD".into(), |c| c.name.clone()),
        container.map_or_else(String::new, |c| c.image.clone()),
    ]
}

fn unsigned(value: i64) -> Option<u64> {
    u64::try_from(value).ok()
}

struct CgroupFiles<'a> {
    group: &'a Root,
    values: HashMap<&'static str, Option<String>>,
}

impl CgroupFiles<'_> {
    fn read(&mut self, file: &'static str) -> Option<&str> {
        self.values
            .entry(file)
            .or_insert_with(|| stats::read_cgroup_file(self.group, file))
            .as_deref()
    }
}

struct DeviceIo {
    device: String,
    major: String,
    minor: String,
    counters: HashMap<String, u64>,
}

fn device_name(major: u32, minor: u32) -> String {
    let uevent = format!("/sys/dev/block/{major}:{minor}/uevent");
    fs::read_to_string(uevent)
        .ok()
        .and_then(|file| {
            file.lines()
                .find_map(|line| line.strip_prefix("DEVNAME="))
                .map(|name| format!("/dev/{name}"))
        })
        .unwrap_or_else(|| format!("{major}:{minor}"))
}

fn devices(stat: &str) -> Vec<DeviceIo> {
    stat.lines()
        .filter_map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let id = fields.next()?;
            let (major, minor) = id.split_once(':')?;
            let major = major.parse::<u32>().ok()?;
            let minor = minor.parse::<u32>().ok()?;
            let device = device_name(major, minor);
            let counters = fields
                .filter_map(|field| {
                    let (key, value) = field.split_once('=')?;
                    Some((key.into(), value.parse().ok()?))
                })
                .collect();
            Some(DeviceIo {
                device,
                major: major.to_string(),
                minor: minor.to_string(),
                counters,
            })
        })
        .collect()
}

const DIRECTORY: OpenFlags = OpenFlags::O_RDONLY
    .union(OpenFlags::O_DIRECTORY)
    .union(OpenFlags::O_NOFOLLOW);

fn writable_bytes(root: &Root, dev: u64, budget: Duration) -> Option<u64> {
    let start = Instant::now();
    let mut directories = vec![PathBuf::from(".")];
    let mut seen = HashSet::new();
    let mut bytes = 0u64;
    while let Some(path) = directories.pop() {
        if start.elapsed() >= budget {
            return None;
        }
        let directory = root.open_subpath(&path, DIRECTORY).ok()?;
        let fd_path = format!("/proc/self/fd/{}", directory.as_raw_fd());
        // Keep the directory fd pinned as entries come and go in the container.
        for entry in fs::read_dir(fd_path).ok()? {
            if start.elapsed() >= budget {
                return None;
            }
            let entry = entry.ok()?;
            let metadata = entry.metadata().ok()?;
            if metadata.dev() != dev || !seen.insert(metadata.ino()) {
                continue;
            }
            bytes = bytes.checked_add(metadata.blocks().checked_mul(512)?)?;
            if metadata.is_dir() {
                directories.push(path.join(entry.file_name()));
            }
        }
    }
    Some(bytes)
}

struct BackingFilesystem {
    device: String,
    inodes: u64,
    inodes_free: u64,
    capacity: u64,
    usage: Option<u64>,
}

fn filesystem_stats(bundle: &Path) -> Option<BackingFilesystem> {
    let bundle = Root::open(bundle).ok()?;
    // OverlayFS charges changes to upper. A non-overlay container owns its
    // materialized rootfs directly.
    let directory = match bundle.open_subpath("upper", DIRECTORY) {
        Ok(directory) => directory,
        Err(error) if error.kind() == ErrorKind::OsError(Some(libc::ENOENT)) => {
            bundle.open_subpath("rootfs", DIRECTORY).ok()?
        }
        Err(_) => return None,
    };
    let fd = directory.as_raw_fd();
    let metadata = fs::metadata(format!("/proc/self/fd/{fd}")).ok()?;
    let dev = metadata.dev();
    let stats = nix::sys::statvfs::fstatvfs(&directory).ok()?;
    let root = Root::from_fd(directory);
    Some(BackingFilesystem {
        device: device_name(libc::major(dev), libc::minor(dev)),
        inodes: stats.files(),
        inodes_free: stats.files_available(),
        capacity: stats.blocks().checked_mul(stats.fragment_size())?,
        usage: writable_bytes(&root, dev, Duration::from_millis(250))
            .and_then(|bytes| bytes.checked_add(metadata.blocks().checked_mul(512)?)),
    })
}

fn fd_counts(pid: u32, cgroup: &Path) -> Option<(u64, u64)> {
    let relative = cgroup.strip_prefix("/sys/fs/cgroup").ok()?;
    let membership = format!("0::/{}", relative.display());
    let process = PathBuf::from(format!("/proc/{pid}"));
    let owns_process = || {
        fs::read_to_string(process.join("cgroup"))
            .is_ok_and(|stat| stat.lines().any(|line| line == membership))
    };
    if !owns_process() {
        return None;
    }
    let mut count = 0;
    let mut sockets = 0;
    for entry in fs::read_dir(process.join("fd")).ok()? {
        let path = entry.ok()?.path();
        match fs::read_link(path) {
            Ok(target) => {
                count += 1;
                if target
                    .as_os_str()
                    .as_encoded_bytes()
                    .starts_with(b"socket:[")
                {
                    sockets += 1;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    owns_process().then_some((count, sockets))
}

fn process_counts(stat: &str, cgroup: &Path) -> Option<(u64, Option<(u64, u64)>)> {
    let pids = stat
        .lines()
        .map(|line| line.parse::<u32>().ok().filter(|pid| *pid > 0))
        .collect::<Option<HashSet<_>>>()?;
    let count = pids.len() as u64;
    let descriptors = pids.into_iter().try_fold((0, 0), |(files, sockets), pid| {
        let (file_count, socket_count) = fd_counts(pid, cgroup)?;
        Some((files + file_count, sockets + socket_count))
    });
    Some((count, descriptors))
}

fn native_metrics(
    group: &Root,
    cgroup: &Path,
    bundle: &Path,
    labels: &[String],
    started: Option<u64>,
) -> Vec<Metric> {
    let mut files = CgroupFiles {
        group,
        values: HashMap::new(),
    };
    let now = unsigned(chrono::Utc::now().timestamp());
    let block = files.read("io.stat").map(devices).unwrap_or_default();
    let filesystem = filesystem_stats(bundle);
    let processes = files
        .read("cgroup.procs")
        .and_then(|stat| process_counts(stat, cgroup));
    let mut result = Vec::new();
    for family in FAMILIES {
        if matches!(family.source, Faults) {
            if let Some(memory) = files.read("memory.stat") {
                // memory.stat already aggregates the entire cgroup hierarchy.
                for key in ["pgfault", "pgmajfault"] {
                    if let Some(value) = stats::stat_value(memory, key) {
                        result.push(family.metric(labels, &[key, "hierarchy"], value));
                    }
                }
            }
            continue;
        }
        if let BlockIo(key) = family.source {
            for device in &block {
                if let Some(&value) = device.counters.get(key) {
                    result.push(family.metric(labels, &[&device.device], value));
                }
            }
            continue;
        }
        if matches!(family.source, BlockBytes) {
            for device in &block {
                for (operation, key) in [
                    ("Read", "rbytes"),
                    ("Write", "wbytes"),
                    ("Discard", "dbytes"),
                ] {
                    if let Some(&value) = device.counters.get(key) {
                        result.push(family.metric(
                            labels,
                            &[&device.device, &device.major, &device.minor, operation],
                            value,
                        ));
                    }
                }
            }
            continue;
        }
        let value = match family.source {
            Number(file) => files.read(file).and_then(|s| s.trim().parse().ok()),
            Field(file, key) => files.read(file).and_then(|s| stats::stat_value(s, key)),
            Seconds(file, key) => files
                .read(file)
                .and_then(|s| stats::stat_value(s, key))
                .map(|us| us / SECOND_US),
            Pressure(file, full) => files
                .read(file)
                .and_then(stats::pressure)
                .and_then(|s| if full { s.full } else { s.some })
                .map(|reading| reading.total / SECOND_NS),
            CpuMax(index) => files
                .read("cpu.max")
                .and_then(|s| s.split_ascii_whitespace().nth(index)?.parse().ok()),
            CpuShares => files
                .read("cpu.weight")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .filter(|weight| (1..=10_000).contains(weight))
                .map(|weight| 2 + (weight - 1) * 262_142 / 9_999),
            WorkingSet => files
                .read("memory.current")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .zip(
                    files
                        .read("memory.stat")
                        .and_then(|s| stats::stat_value(s, "inactive_file")),
                )
                .map(|(usage, inactive)| usage.saturating_sub(inactive)),
            LastSeen => now,
            Started => started,
            Network(_) => None,
            Processes => processes.map(|counts| counts.0),
            Fds => processes.and_then(|counts| counts.1).map(|counts| counts.0),
            Sockets => processes.and_then(|counts| counts.1).map(|counts| counts.1),
            ThreadLimit => files.read("pids.max").and_then(|limit| match limit.trim() {
                "max" => Some(0),
                finite => finite.parse().ok(),
            }),
            MemoryLimit(file) => files.read(file).and_then(|limit| match limit.trim() {
                "max" => Some(0),
                finite => finite
                    .parse::<u64>()
                    .ok()
                    .map(|value| if value > 1 << 62 { 0 } else { value }),
            }),
            Filesystem(kind) => {
                if let Some(filesystem) = &filesystem {
                    let value = match kind {
                        FsValue::Inodes => Some(filesystem.inodes),
                        FsValue::InodesFree => Some(filesystem.inodes_free),
                        FsValue::Limit => Some(filesystem.capacity),
                        FsValue::Usage => filesystem.usage,
                    };
                    if let Some(value) = value {
                        result.push(family.metric(labels, &[&filesystem.device], value));
                    }
                }
                continue;
            }
            Faults | BlockIo(_) | BlockBytes => {
                unreachable!("multi-series values are emitted separately")
            }
        };
        if let Some(value) = value {
            let extra = if family.name == CPU_USAGE.name {
                &["total"][..]
            } else {
                &[]
            };
            result.push(family.metric(labels, extra, value));
        }
    }
    result
}

fn network_metrics(stat: &str, labels: &[String]) -> Vec<Metric> {
    let mut result = Vec::new();
    for line in stat.lines() {
        let Some((name, counters)) = line.rsplit_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let counters: Vec<_> = counters.split_ascii_whitespace().collect();
        for family in FAMILIES {
            if let Network(index) = family.source {
                if let Some(value) = counters.get(index).and_then(|v| v.parse().ok()) {
                    result.push(family.metric(labels, &[name], value));
                }
            }
        }
    }
    result
}

fn pod_network_stat(pid: i32, owned_netns: &Path) -> Option<String> {
    let pid_netns = PathBuf::from(format!("/proc/{pid}/ns/net"));
    let identity = |path: &Path| {
        let m = fs::metadata(path).ok()?;
        Some((m.dev(), m.ino()))
    };
    let expected = identity(owned_netns)?;
    if identity(&pid_netns)? != expected {
        return None;
    }
    let stat = fs::read_to_string(format!("/proc/{pid}/net/dev")).ok()?;
    (identity(&pid_netns)? == expected && identity(owned_netns)? == expected).then_some(stat)
}

fn base_metrics(
    cpu: Option<CpuUsage>,
    memory: Option<MemoryUsage>,
    labels: &[String],
) -> Vec<Metric> {
    let cpu = cpu.and_then(|sample| sample.usage_core_nano_seconds);
    let (working_set, usage) = memory
        .map(|m| (m.working_set_bytes, m.usage_bytes))
        .unwrap_or_default();
    [
        (
            &CPU_USAGE,
            cpu.map(|value| value.value / SECOND_NS),
            &["total"][..],
        ),
        (
            &MEMORY_WORKING_SET,
            working_set.map(|value| value.value),
            &[],
        ),
        (&MEMORY_USAGE, usage.map(|value| value.value), &[]),
    ]
    .into_iter()
    .filter_map(|(family, value, extra)| Some(family.metric(labels, extra, value?)))
    .collect()
}

impl NautilusRuntimeService {
    async fn native_pod_network_metrics(&self, pod: &PodState) -> Vec<Metric> {
        let Some(pid) = pod.netns_holder_pid.filter(|pid| *pid > 0) else {
            return Vec::new();
        };
        let owned_path = self._config.network.netns_dir.join(&pod.id);
        if pod.status != PodStatus::Running
            || pod.network_namespace.as_deref().map(Path::new) != Some(owned_path.as_path())
        {
            return Vec::new();
        }
        let labels = labels(pod, None);
        tokio::task::spawn_blocking(move || {
            pod_network_stat(pid, &owned_path)
                .map(|stat| network_metrics(&stat, &labels))
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    }

    pub(super) async fn collect_pod_metrics(
        &self,
        pod: &PodState,
    ) -> Result<PodSandboxMetrics, Status> {
        let containers = self
            ._state_manager
            .list_containers_in_pod(&pod.id)
            .await
            .map_err(Status::from)?;
        if pod.runtime_handler == "vm" {
            let sample = self
                .guest_pod_stats(pod)
                .await?
                .linux
                .ok_or_else(|| Status::data_loss("guest metrics require Linux stats"))?;
            let container_metrics = sample
                .containers
                .into_iter()
                .filter_map(|sample| {
                    let id = sample.attributes?.id;
                    let container = containers.iter().find(|c| c.id == id)?;
                    let metrics =
                        base_metrics(sample.cpu, sample.memory, &labels(pod, Some(container)));
                    Some(ContainerMetrics {
                        container_id: id,
                        metrics,
                    })
                })
                .collect();
            return Ok(PodSandboxMetrics {
                pod_sandbox_id: pod.id.clone(),
                metrics: base_metrics(sample.cpu, sample.memory, &labels(pod, None)),
                container_metrics,
            });
        }
        let container_metrics = futures::stream::iter(containers)
            .map(|container| async move {
                let series_labels = labels(pod, Some(&container));
                let started = container
                    .started_at
                    .as_ref()
                    .and_then(|time| unsigned(time.timestamp()));
                let metrics = match self
                    .read_native_cgroup(&container, {
                        let labels = series_labels.clone();
                        move |group, path, bundle| {
                            native_metrics(group, path, bundle, &labels, started)
                        }
                    })
                    .await
                    .ok()?
                {
                    Some(metrics) => metrics,
                    None => {
                        let sample = self.local_container_stats(&container).await.ok()?;
                        base_metrics(sample.cpu, sample.memory, &series_labels)
                    }
                };
                Some(ContainerMetrics {
                    container_id: container.id,
                    metrics,
                })
            })
            .buffered(4)
            .filter_map(std::future::ready)
            .collect()
            .await;
        Ok(PodSandboxMetrics {
            pod_sandbox_id: pod.id.clone(),
            metrics: self.native_pod_network_metrics(pod).await,
            container_metrics,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn native_descriptor_labels_values_and_units_match() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cpu.stat"),
            "usage_usec 2500000\nnr_throttled 0\nthrottled_usec 1500000\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("memory.stat"),
            "pgfault 6\npgmajfault 2\nanon 1000\ninactive_file 1001\n",
        )
        .unwrap();
        fs::write(dir.path().join("memory.current"), "1000\n").unwrap();
        fs::write(dir.path().join("memory.swap.max"), "max\n").unwrap();
        fs::write(
            dir.path().join("cpu.pressure"),
            "some avg10=0 avg60=0 avg300=0 total=2000000\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("io.stat"),
            "8:0 rbytes=8192 wbytes=0 rios=2\ninvalid rbytes=0\n",
        )
        .unwrap();
        let group = Root::open(dir.path()).unwrap();
        fs::write(dir.path().join("cgroup.procs"), "1\n2\n1\n").unwrap();
        fs::write(dir.path().join("pids.max"), "max\n").unwrap();
        let series = native_metrics(
            &group,
            Path::new("/sys/fs/cgroup/nautilus-invalid-test-scope"),
            dir.path(),
            &LABELS.map(String::from),
            Some(12),
        );
        let descriptors: HashMap<_, _> = descriptors()
            .into_iter()
            .map(|d| (d.name.clone(), d))
            .collect();
        for metric in &series {
            assert_eq!(
                descriptors[&metric.name].label_keys.len(),
                metric.label_values.len()
            );
            assert_eq!(metric.timestamp, 0);
        }
        let metric = |name| series.iter().find(|s| s.name == name).unwrap();
        let value = |name| metric(name).value.as_ref().unwrap().value;
        assert_eq!(value(CPU_USAGE.name), 2);
        assert_eq!(metric(CPU_USAGE.name).label_values.last().unwrap(), "total");
        assert_eq!(value("container_cpu_cfs_throttled_periods_total"), 0);
        assert_eq!(value("container_cpu_cfs_throttled_seconds_total"), 1);
        assert_eq!(value("container_memory_working_set_bytes"), 0);
        assert_eq!(value("container_pressure_cpu_waiting_seconds_total"), 2);
        assert_eq!(value("container_start_time_seconds"), 12);
        assert_eq!(value("container_processes"), 2);
        assert_eq!(value("container_threads_max"), 0);
        assert_eq!(value("container_spec_memory_swap_limit_bytes"), 0);
        assert!(series
            .iter()
            .all(|s| s.name != "container_file_descriptors" && s.name != "container_sockets"));
        assert_eq!(value("container_fs_reads_bytes_total"), 8192);
        assert_eq!(value("container_fs_writes_bytes_total"), 0);
        let block = metric("container_blkio_device_usage_total");
        assert_eq!(block.metric_type, Counter as i32);
        assert_eq!(block.label_values[LABELS.len() + 1..], ["8", "0", "Read"]);
        assert!(series
            .iter()
            .all(|s| s.name != "container_memory_max_usage_bytes"
                && s.name != "container_spec_memory_limit_bytes"));
        let faults: Vec<_> = series
            .iter()
            .filter(|s| s.name == "container_memory_failures_total")
            .collect();
        assert_eq!(faults.len(), 2);
        assert_eq!(
            faults[0].label_values[LABELS.len()..],
            ["pgfault", "hierarchy"]
        );
        assert_eq!(faults[1].value.as_ref().unwrap().value, 2);
    }

    #[test]
    fn pod_network_stats_keep_interface_and_rx_tx_fields() {
        let series = network_metrics(
            "Inter-| Receive | Transmit\n  eth0: 1 2 3 4 0 0 0 0  11 12 13 14 0 0 0 0\n",
            &LABELS.map(String::from),
        );
        assert_eq!(series.len(), 8);
        for metric in &series {
            assert_eq!(&metric.label_values[LABELS.len()..], ["eth0"]);
        }
        let value = |name| {
            series
                .iter()
                .find(|c| c.name == name)
                .unwrap()
                .value
                .as_ref()
                .unwrap()
                .value
        };
        assert_eq!(value("container_network_receive_errors_total"), 3);
        assert_eq!(value("container_network_transmit_bytes_total"), 11);
        assert_eq!(
            value("container_network_transmit_packets_dropped_total"),
            14
        );
    }

    #[test]
    fn writable_snapshot_counts_allocated_blocks_without_following_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        fs::create_dir(&upper).unwrap();
        let file = upper.join("data");
        fs::write(&file, vec![1; 8192]).unwrap();
        fs::hard_link(&file, upper.join("same-data")).unwrap();
        std::os::unix::fs::symlink(dir.path(), upper.join("outside")).unwrap();
        let expected = fs::metadata(&upper).unwrap().blocks() * 512
            + fs::metadata(file).unwrap().blocks() * 512
            + fs::symlink_metadata(upper.join("outside"))
                .unwrap()
                .blocks()
                * 512;
        let sampled = filesystem_stats(dir.path()).unwrap();
        assert_eq!(sampled.usage, Some(expected));
        assert!(sampled.capacity >= expected);
    }
}
