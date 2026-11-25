use super::{Driver, RuncBackend};
use crate::core::state::{NativeResizeControl as Control, NativeResizeValue};
use crate::core::{Error, Result};
use crate::sandbox::ResourceLimits;
use pathrs::{flags::OpenFlags, Root};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

impl ResourceLimits {
    pub(crate) fn with_native_resize(&self, patch: &Self) -> Result<Self> {
        if patch.cpu_period != 0 && !(1_000..=1_000_000).contains(&patch.cpu_period) {
            return Err(Error::Config(
                "CPU period must be 1000–1000000 microseconds".into(),
            ));
        }
        let max_quota = (u64::MAX / 1_000_000) as i64;
        if patch.cpu_quota != 0
            && patch.cpu_quota != -1
            && !(1_000..=max_quota).contains(&patch.cpu_quota)
        {
            return Err(Error::Config(
                "CPU quota is outside the supported range".into(),
            ));
        }
        if patch.cpu_shares != 0 && !(2..=262_144).contains(&patch.cpu_shares) {
            return Err(Error::Config("CPU shares must be 2–262144".into()));
        }
        if patch.memory_limit_in_bytes < -1 {
            return Err(Error::Config("memory limit must be positive or -1".into()));
        }

        let mut next = self.clone();
        if patch.cpu_period != 0 {
            next.cpu_period = patch.cpu_period;
        }
        if patch.cpu_quota != 0 {
            next.cpu_quota = patch.cpu_quota;
        }
        if patch.cpu_shares != 0 {
            next.cpu_shares = patch.cpu_shares;
        }
        if patch.memory_limit_in_bytes != 0 {
            next.memory_limit_in_bytes = patch.memory_limit_in_bytes;
        }
        next.native_disable_swap |= patch.native_disable_swap;
        next.native_oom_group = patch.native_oom_group.or(self.native_oom_group);
        Ok(next)
    }
}

impl RuncBackend {
    fn resize_cgroup(&self, id: &str, pid: u32) -> Result<LiveResize> {
        if self.driver() != Driver::Libcontainer
            || !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file()
        {
            return Err(Error::Experimental(
                "live OCI resize requires libcontainer and cgroup v2".into(),
            ));
        }
        let (path, systemd) = self.workload_cgroup_v2_owner(id, pid).ok_or_else(|| {
            Error::Precondition("could not verify a running container's own cgroup".into())
        })?;
        let mut live = LiveResize::new(Root::open(&path)?);
        if systemd {
            let unit = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| Error::Runtime("could not determine owned systemd scope".into()))?;
            live.unit = Some(unit.to_owned());
        }
        Ok(live)
    }

    pub(crate) fn prepare_resize(
        &self,
        id: &str,
        pid: u32,
        patch: &ResourceLimits,
    ) -> Result<LiveResize> {
        let mut live = self.resize_cgroup(id, pid)?;
        live.plan(patch)?;
        Ok(live)
    }

    pub(crate) fn restore_resize(
        &self,
        id: &str,
        pid: u32,
        original: &[NativeResizeValue],
    ) -> Result<()> {
        if original.is_empty() {
            return Ok(());
        }
        self.resize_cgroup(id, pid)?.restore(original)
    }
}

struct Change {
    original: NativeResizeValue,
    next: String,
}

/// A plan captures every original control before the daemon persists the rollback record.
pub(crate) struct LiveResize {
    group: Root,
    unit: Option<String>,
    changes: Vec<Change>,
    frozen: Option<String>,
}

impl LiveResize {
    fn new(group: Root) -> Self {
        Self {
            group,
            unit: None,
            changes: Vec::new(),
            frozen: None,
        }
    }

    fn plan(&mut self, patch: &ResourceLimits) -> Result<()> {
        if patch.cpu_quota != 0 || patch.cpu_period != 0 {
            self.change(Control::CpuMax, |previous| {
                cpu_max(previous, patch.cpu_quota, patch.cpu_period)
            })?;
        }
        if patch.cpu_shares != 0 {
            self.change(Control::CpuWeight, |_| {
                Ok(cpu_weight(patch.cpu_shares).to_string())
            })?;
        }
        if patch.native_disable_swap {
            self.change(Control::SwapLimit, |_| Ok("0".into()))?;
        }
        if let Some(oom_group) = patch.native_oom_group {
            let value = if oom_group { "1" } else { "0" };
            self.change(Control::OomGroup, |_| Ok(value.into()))?;
        }
        if patch.memory_limit_in_bytes != 0 {
            let desired = memory_max(patch.memory_limit_in_bytes)?;
            let previous = read_control(&self.group, Control::MemoryLimit.file())?;
            if desired != previous {
                self.check_memory_limit(&previous, &desired)?;
                self.frozen = Some(read_control(&self.group, Control::Frozen.file())?);
                self.changes.push(Change {
                    original: NativeResizeValue {
                        control: Control::MemoryLimit,
                        value: previous,
                    },
                    next: desired,
                });
            }
        }
        Ok(())
    }

    fn change(
        &mut self,
        control: Control,
        requested: impl FnOnce(&str) -> Result<String>,
    ) -> Result<()> {
        let value = read_control(&self.group, control.file())?;
        let next = requested(&value)?;
        if value != next {
            self.changes.push(Change {
                original: NativeResizeValue { control, value },
                next,
            });
        }
        Ok(())
    }

    pub(crate) fn original_values(&self) -> Vec<NativeResizeValue> {
        self.changes
            .iter()
            .map(|change| change.original.clone())
            .chain(self.frozen.iter().map(|value| NativeResizeValue {
                control: Control::Frozen,
                value: value.clone(),
            }))
            .collect()
    }

    pub(crate) fn apply(&self) -> Result<()> {
        // Keep the cgroup frozen until the resource commit. An increase can
        // otherwise let the workload allocate past the old limit before the
        // daemon can guarantee it will never need to restore that limit.
        if self.frozen.is_some() {
            self.freeze()?;
        }
        if let Some(memory) = self
            .changes
            .iter()
            .find(|c| c.original.control == Control::MemoryLimit)
        {
            self.check_memory_limit(&memory.original.value, &memory.next)?;
        }
        for change in &self.changes {
            self.write_and_check(change.original.control, &change.next)?;
        }
        // Updating another systemd unit property can reapply a previously set
        // CPU bandwidth in systemd's rounded units. Verify it after all of them.
        if let Some(cpu) = self
            .changes
            .iter()
            .find(|c| c.original.control == Control::CpuMax)
            .filter(|_| self.unit.is_some())
        {
            self.write_kernel_and_check(Control::CpuMax, &cpu.next)?;
        }
        Ok(())
    }

    pub(crate) fn restore(&self, original: &[NativeResizeValue]) -> Result<()> {
        let mut failures = Vec::new();
        for value in original
            .iter()
            .rev()
            .filter(|value| value.control != Control::Frozen)
            .chain(
                original
                    .iter()
                    .filter(|value| value.control == Control::Frozen),
            )
        {
            if let Err(error) = self.restore_one(value) {
                failures.push(format!("{}: {error}", value.control.file()));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::Runtime(failures.join(", ")))
        }
    }

    fn restore_one(&self, original: &NativeResizeValue) -> Result<()> {
        let current = read_control(&self.group, original.control.file())?;
        if original.control == Control::MemoryLimit && limit(&original.value)? < limit(&current)? {
            self.freeze()?;
            self.check_memory_limit(&current, &original.value)?;
        }
        self.write_and_check(original.control, &original.value)
    }

    fn check_memory_limit(&self, previous: &str, desired: &str) -> Result<()> {
        if limit(desired)? < limit(previous)? {
            let usage = read_control(&self.group, "memory.current")?;
            if limit(desired)? < limit(&usage)? {
                return Err(Error::Precondition(format!(
                    "memory limit {desired} is below current usage {usage}"
                )));
            }
        }
        Ok(())
    }

    fn freeze(&self) -> Result<()> {
        self.write_and_check(Control::Frozen, "1")?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if read_control(&self.group, "cgroup.events")?.contains("frozen 1") {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::Precondition(
                    "could not freeze cgroup for memory resize".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn write_and_check(&self, control: Control, value: &str) -> Result<()> {
        if let Some(unit) = &self.unit {
            let properties = control.systemd_properties(value)?;
            if !properties.is_empty() {
                let mut bus = std::process::Command::new("busctl");
                bus.args([
                    "--quiet",
                    "--timeout=5s",
                    "call",
                    "org.freedesktop.systemd1",
                    "/org/freedesktop/systemd1",
                    "org.freedesktop.systemd1.Manager",
                    "SetUnitProperties",
                    "sba(sv)",
                    unit,
                    "true",
                    &properties.len().to_string(),
                ]);
                for (name, value) in properties {
                    bus.args([name, "t", &value.to_string()]);
                }
                let output = bus.output()?;
                if !output.status.success() {
                    return Err(Error::Runtime(format!(
                        "update systemd scope {unit}: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )));
                }
            }
        }
        self.write_kernel_and_check(control, value)
    }

    fn write_kernel_and_check(&self, control: Control, value: &str) -> Result<()> {
        let name = control.file();
        if read_control(&self.group, name)? != value {
            let mut file = self.group.open_subpath(
                name,
                OpenFlags::O_WRONLY | OpenFlags::O_TRUNC | OpenFlags::O_NONBLOCK,
            )?;
            file.write_all(value.as_bytes())?;
        }
        let observed = read_control(&self.group, name)?;
        if observed != value {
            return Err(Error::Runtime(format!(
                "cgroup {name} read back {observed}, expected {value}"
            )));
        }
        Ok(())
    }
}

fn read_control(group: &Root, name: &str) -> Result<String> {
    let mut contents = String::new();
    group
        .open_subpath(name, OpenFlags::O_RDONLY)?
        .read_to_string(&mut contents)?;
    Ok(contents.split_whitespace().collect::<Vec<_>>().join(" "))
}

impl Control {
    fn file(self) -> &'static str {
        match self {
            Self::CpuMax => "cpu.max",
            Self::CpuWeight => "cpu.weight",
            Self::MemoryLimit => "memory.max",
            Self::SwapLimit => "memory.swap.max",
            Self::OomGroup => "memory.oom.group",
            Self::Frozen => "cgroup.freeze",
        }
    }

    fn systemd_properties(self, value: &str) -> Result<Vec<(&'static str, u64)>> {
        let property = match self {
            Self::CpuMax => {
                let (quota, period) = value
                    .split_once(' ')
                    .ok_or_else(|| Error::Runtime("could not parse cpu.max".into()))?;
                let period = limit(period)?;
                let quota = if quota == "max" {
                    u64::MAX
                } else {
                    limit(quota)?
                        .checked_mul(1_000_000)
                        .and_then(|quota| quota.checked_div(period))
                        .ok_or_else(|| Error::Runtime("invalid CPU quota or period".into()))?
                };
                return Ok(vec![
                    ("CPUQuotaPeriodUSec", period),
                    ("CPUQuotaPerSecUSec", quota),
                ]);
            }
            Self::CpuWeight => ("CPUWeight", limit(value)?),
            Self::MemoryLimit => ("MemoryMax", limit(value)?),
            Self::SwapLimit => ("MemorySwapMax", limit(value)?),
            Self::OomGroup | Self::Frozen => return Ok(Vec::new()),
        };
        Ok(vec![property])
    }
}

fn cpu_max(current: &str, quota: i64, period: i64) -> Result<String> {
    let (old_quota, old_period) = current
        .split_once(' ')
        .ok_or_else(|| Error::Runtime("could not parse cpu.max".into()))?;
    let quota = match quota {
        0 => old_quota.to_owned(),
        -1 => "max".into(),
        value => value.to_string(),
    };
    let period = if period == 0 {
        old_period.to_owned()
    } else {
        period.to_string()
    };
    Ok(format!("{quota} {period}"))
}

// Same quadratic mapping as libcgroups 0.8's systemd and cgroupfs drivers.
fn cpu_weight(shares: i64) -> u64 {
    let shares = (shares as f64).log2();
    let exponent = (shares * shares + 125.0 * shares) / 612.0 - 7.0 / 34.0;
    (10f64.powf(exponent).ceil() as u64).clamp(1, 10_000)
}

fn memory_max(bytes: i64) -> Result<String> {
    if bytes == -1 {
        return Ok("max".into());
    }
    // The kernel's page counter rounds a byte value down to a whole page.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return Err(Error::Runtime(
            "could not determine system page size".into(),
        ));
    }
    if bytes < page {
        return Err(Error::Config(format!(
            "memory limit must be at least one page ({page} bytes)"
        )));
    }
    Ok((bytes - bytes % page).to_string())
}

fn limit(value: &str) -> Result<u64> {
    if value == "max" {
        return Ok(u64::MAX);
    }
    value
        .parse()
        .map_err(|e| Error::Runtime(format!("invalid cgroup limit {value}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_preserves_omitted_settings_and_can_remove_limits() {
        let old = ResourceLimits {
            cpu_period: 100_000,
            cpu_quota: 20_000,
            cpuset_cpus: "1,2".into(),
            memory_limit_in_bytes: 64 << 20,
            ..Default::default()
        };
        let next = old
            .with_native_resize(&ResourceLimits {
                cpu_quota: -1,
                memory_limit_in_bytes: -1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(next.cpu_period, old.cpu_period);
        assert_eq!(next.cpuset_cpus, old.cpuset_cpus);
        assert_eq!(next.cpu_quota, -1);
        assert_eq!(next.memory_limit_in_bytes, -1);
        assert_eq!(cpu_weight(1024), 100);
    }

    #[test]
    fn live_resize_rolls_back_cpu_when_a_later_control_rejects_the_write() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cpu.max"), "20000 100000\n").unwrap();
        std::fs::write(dir.path().join("cpu.weight"), "42\n").unwrap();
        let mut live = LiveResize::new(Root::open(dir.path()).unwrap());
        live.plan(&ResourceLimits {
            cpu_quota: 40_000,
            cpu_shares: 1024,
            ..Default::default()
        })
        .unwrap();
        let originals = live.original_values();
        // Simulate a control disappearing after validation succeeded.
        std::fs::remove_file(dir.path().join("cpu.weight")).unwrap();
        assert!(live.apply().is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cpu.max")).unwrap(),
            "40000 100000"
        );
        // A fresh worker can use the durable snapshot after an interrupted update.
        let recovered = LiveResize::new(Root::open(dir.path()).unwrap());
        assert!(recovered.restore(&originals).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cpu.max")).unwrap(),
            "20000 100000"
        );
        std::fs::write(dir.path().join("cpu.weight"), "100").unwrap();
        recovered.restore(&originals).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cpu.weight")).unwrap(),
            "42"
        );
    }

    #[test]
    fn unsafe_memory_shrink_leaves_cpu_and_memory_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        for (name, value) in [
            ("cpu.max", "20000 100000"),
            ("memory.max", "67108864"),
            ("memory.current", "25165824"),
        ] {
            std::fs::write(dir.path().join(name), value).unwrap();
        }
        let patch = ResourceLimits {
            cpu_quota: 40_000,
            memory_limit_in_bytes: 8 << 20,
            ..Default::default()
        };
        let mut live = LiveResize::new(Root::open(dir.path()).unwrap());
        assert!(matches!(live.plan(&patch), Err(Error::Precondition(_))));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cpu.max")).unwrap(),
            "20000 100000"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("memory.max")).unwrap(),
            "67108864"
        );
    }

    #[test]
    fn committed_memory_update_leaves_only_the_freezer_to_recover() {
        let dir = tempfile::tempdir().unwrap();
        for (name, value) in [
            ("memory.max", "67108864"),
            ("cgroup.freeze", "0"),
            ("cgroup.events", "populated 1\nfrozen 1\n"),
        ] {
            std::fs::write(dir.path().join(name), value).unwrap();
        }
        let mut live = LiveResize::new(Root::open(dir.path()).unwrap());
        live.plan(&ResourceLimits {
            memory_limit_in_bytes: 128 << 20,
            ..Default::default()
        })
        .unwrap();
        live.apply().unwrap();
        assert_eq!(read_control(&live.group, "cgroup.freeze").unwrap(), "1");
        let mut committed = live.original_values();
        committed.retain(|value| value.control == Control::Frozen);
        let recovered = LiveResize::new(Root::open(dir.path()).unwrap());
        recovered.restore(&committed).unwrap();
        assert_eq!(
            read_control(&recovered.group, "cgroup.freeze").unwrap(),
            "0"
        );
        assert_eq!(
            read_control(&recovered.group, "memory.max").unwrap(),
            "134217728"
        );
    }
}
