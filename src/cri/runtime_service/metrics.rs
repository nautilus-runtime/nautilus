use crate::proto::runtime::v1::{
    ContainerMetrics, CpuUsage, MemoryUsage, Metric, MetricDescriptor, MetricType,
    PodSandboxMetrics, PodSandboxStats,
};

const CPU: &str = "nautilus_cri_cpu_usage_nanoseconds_total";
const MEMORY: &str = "nautilus_cri_memory_working_set_bytes";
const LABELS: [&str; 2] = ["id", "scope"];

pub(super) fn descriptors() -> Vec<MetricDescriptor> {
    [
        (CPU, "CPU time accumulated by the workload in nanoseconds."),
        (MEMORY, "Current workload memory working set in bytes."),
    ]
    .into_iter()
    .map(|(name, help)| MetricDescriptor {
        name: name.into(),
        help: help.into(),
        label_keys: LABELS.map(String::from).into(),
    })
    .collect()
}

fn metrics(
    cpu: Option<CpuUsage>,
    memory: Option<MemoryUsage>,
    id: &str,
    scope: &str,
) -> Vec<Metric> {
    let cpu = cpu.and_then(|sample| sample.usage_core_nano_seconds);
    let memory = memory.and_then(|sample| sample.working_set_bytes);
    [
        (CPU, MetricType::Counter, cpu),
        (MEMORY, MetricType::Gauge, memory),
    ]
    .into_iter()
    .filter_map(|(name, kind, value)| {
        Some(Metric {
            name: name.into(),
            timestamp: 0,
            metric_type: kind as i32,
            label_values: vec![id.into(), scope.into()],
            value: Some(value?),
        })
    })
    .collect()
}

pub(super) fn pod_metrics(stats: PodSandboxStats) -> Option<PodSandboxMetrics> {
    let linux = stats.linux?;
    let pod_id = stats.attributes?.id;
    Some(PodSandboxMetrics {
        metrics: metrics(linux.cpu, linux.memory, &pod_id, "pod"),
        pod_sandbox_id: pod_id,
        container_metrics: linux
            .containers
            .into_iter()
            .filter_map(|container| {
                let id = container.attributes?.id;
                Some(ContainerMetrics {
                    metrics: metrics(container.cpu, container.memory, &id, "container"),
                    container_id: id,
                })
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::runtime::v1::UInt64Value;

    #[test]
    fn absent_counters_are_omitted_and_live_zero_is_kept() {
        let data = metrics(
            Some(CpuUsage {
                usage_core_nano_seconds: Some(UInt64Value { value: 0 }),
                ..Default::default()
            }),
            None,
            "test-id",
            "pod",
        );
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].name, descriptors()[0].name);
        assert_eq!(data[0].metric_type, MetricType::Counter as i32);
        assert_eq!(data[0].timestamp, 0);
        assert_eq!(data[0].value.as_ref().unwrap().value, 0);
        assert_eq!(data[0].label_values, ["test-id", "pod"]);
    }
}
