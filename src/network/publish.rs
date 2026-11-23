//! Docker-style host port publishing (`-p [hostIp:]hostPort:containerPort[/proto]`).
//!
//! Rules live in the nat table under a dedicated `NAUTILUS-PUB` chain hooked
//! into PREROUTING and OUTPUT. Every rule carries a `naut:<container-id>`
//! comment so cleanup (and the startup sweep) can find exactly its own rules,
//! surviving daemon restarts. The chain itself is shared and left installed.
//!
//! A MASQUERADE companion rule per mapping handles hairpin traffic: replies to
//! loopback-sourced clients would otherwise die inside the container netns,
//! and same-bridge clients need the source rewritten to reach a peer veth.

use crate::core::error::{Error, Result};
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::LazyLock;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

pub const PUBLISH_LABEL: &str = "nautilus.dev/publish";

const CHAIN: &str = "NAUTILUS-PUB";
/// OUTPUT-side twin: locally-generated traffic traverses nat OUTPUT before
/// local delivery, where a DNAT would preempt the loopback forwarder. Ports
/// served by a forwarder get NO rule here so the listener sees them.
const CHAIN_OUT: &str = "NAUTILUS-PUB-OUT";
const COMMENT_PREFIX: &str = "naut:";

#[derive(Debug, Clone, serde::Serialize)]
pub struct PortMapping {
    /// Host IP to bind; empty string means all interfaces (docker's 0.0.0.0).
    #[serde(default)]
    pub host_ip: String,
    pub host_port: u16,
    pub ctr_port: u16,
    /// "tcp" | "udp"
    pub proto: String,
}

impl PortMapping {
    /// Parse one docker-style spec: `[hostIp:]hostPort:containerPort[/proto]`.
    pub fn parse(spec: &str) -> Result<Self> {
        let s = spec.trim();
        if s.is_empty() {
            return Err(Error::Runtime("empty -p spec".to_string()));
        }
        let (body, proto) = match s.rsplit_once('/') {
            Some((b, p)) => (b, p.to_ascii_lowercase()),
            None => (s, "tcp".to_string()),
        };
        if !matches!(proto.as_str(), "tcp" | "udp") {
            return Err(Error::Runtime(format!("unsupported proto {proto:?}")));
        }
        // NOTE: IPv6 literal hosts are not supported in this MVP.
        let parts: Vec<&str> = body.split(':').collect();
        let (host_ip, host_port, ctr_port) = match parts.as_slice() {
            [hp, cp] => (String::new(), hp.to_string(), cp.to_string()),
            [ip, hp, cp] => ((*ip).to_string(), hp.to_string(), cp.to_string()),
            _ => {
                return Err(Error::Runtime(format!(
                    "bad -p spec {spec:?}: want [hostIp:]hostPort:containerPort[/proto]"
                )));
            }
        };
        let host_port: u16 = host_port
            .parse()
            .map_err(|_| Error::Runtime(format!("bad host port in {spec:?}")))?;
        let ctr_port: u16 = ctr_port
            .parse()
            .map_err(|_| Error::Runtime(format!("bad container port in {spec:?}")))?;
        Ok(Self {
            host_ip,
            host_port,
            ctr_port,
            proto,
        })
    }

    /// Parse a comma-joined canonical list (as stored in the publish label).
    pub fn parse_list(spec: &str) -> Vec<Self> {
        spec.split(',')
            .filter(|s| !s.trim().is_empty())
            .filter_map(|s| Self::parse(s).ok())
            .collect()
    }

    /// Canonical storage form (what goes into the label / state).
    pub fn encode_all(maps: &[PortMapping]) -> String {
        maps.iter()
            .map(|m| {
                let ip = if m.host_ip.is_empty() {
                    String::new()
                } else {
                    format!("{}:", m.host_ip)
                };
                format!("{ip}{}:{}/{}", m.host_port, m.ctr_port, m.proto)
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// docker ps style display: `0.0.0.0:8080->80/tcp`.
    pub fn display(&self) -> String {
        let ip = if self.host_ip.is_empty() {
            "0.0.0.0"
        } else {
            &self.host_ip
        };
        format!("{}:{}->{}{}", ip, self.host_port, self.ctr_port, self.proto)
    }
}

fn run_ipt(args: &[&str]) -> Result<()> {
    let out = Command::new("iptables")
        .args(["-w", "-t", "nat"])
        .args(args)
        .output()
        .map_err(|e| Error::Runtime(format!("spawn iptables: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(Error::Runtime(format!(
            "iptables {} failed: {}",
            args.join(" "),
            err.trim()
        )));
    }
    Ok(())
}

fn ensure_route_localnet() {
    // Loopback-sourced clients (curl 127.0.0.1:hostPort) produce a forwarded
    // packet whose source is 127/8 after DNAT; the kernel drops those as
    // martian unless route_localnet is enabled. Scope-limited best effort.
    let _ = Command::new("sysctl")
        .args(["-w", "net.ipv4.conf.all.route_localnet=1"])
        .output();
}

fn ensure_chains(mut ipt: impl FnMut(&[&str]) -> Result<()>) -> Result<()> {
    for (source, target) in [("PREROUTING", CHAIN), ("OUTPUT", CHAIN_OUT)] {
        if ipt(&["-S", target]).is_err() {
            if let Err(err) = ipt(&["-N", target]) {
                // Another instance may have created the shared chain concurrently.
                ipt(&["-S", target]).or(Err(err))?;
            }
        }
        if ipt(&["-C", source, "-j", target]).is_err() {
            ipt(&["-A", source, "-j", target])?;
        }
    }
    Ok(())
}

impl PortMapping {
    fn host_address(&self) -> Option<&str> {
        match self.host_ip.as_str() {
            "" | "0.0.0.0" => None,
            "localhost" => Some("127.0.0.1"),
            address => Some(address),
        }
    }

    fn uses_loopback_forwarder(&self) -> bool {
        self.proto == "tcp" && matches!(self.host_address(), None | Some("127.0.0.1"))
    }

    fn install(
        &self,
        pod_ip: &str,
        comment: &str,
        mut ipt: impl FnMut(&[&str]) -> Result<()>,
    ) -> Result<()> {
        let dst = format!("{pod_ip}:{}", self.ctr_port);
        let dport = self.host_port.to_string();
        for chain in [CHAIN, CHAIN_OUT] {
            let mut args = vec!["-A", chain];
            if chain == CHAIN_OUT && self.uses_loopback_forwarder() {
                if self.host_address().is_some() {
                    continue; // Only the listener should receive traffic for this mapping.
                }
                args.extend(["!", "-d", "127.0.0.1"]);
            } else if let Some(address) = self.host_address() {
                args.extend(["-d", address]);
            }
            args.extend([
                "-p",
                &self.proto,
                "-m",
                "addrtype",
                "--dst-type",
                "LOCAL",
                "--dport",
                &dport,
                "-m",
                "comment",
                "--comment",
                comment,
                "-j",
                "DNAT",
                "--to-destination",
                &dst,
            ]);
            ipt(&args)?;
        }

        let pod_net = format!("{pod_ip}/32");
        let container_port = self.ctr_port.to_string();
        ipt(&[
            "-A",
            "POSTROUTING",
            "-d",
            &pod_net,
            "-p",
            &self.proto,
            "--dport",
            &container_port,
            "-m",
            "comment",
            "--comment",
            comment,
            "-j",
            "MASQUERADE",
        ])
    }
}

/// Install DNAT+MASQUERADE rules for a container or a pod.
pub async fn apply(pod_ip: &str, owner: &str, maps: &[PortMapping]) -> Result<()> {
    start_forwarders(pod_ip, owner, maps).await?;
    let install = || -> Result<()> {
        ensure_chains(run_ipt)?;
        ensure_route_localnet();
        let comment = format!("{COMMENT_PREFIX}{owner}");
        for mapping in maps {
            mapping.install(pod_ip, &comment, |args| {
                let mut check = args.to_vec();
                check[0] = "-C";
                if run_ipt(&check).is_err() {
                    run_ipt(args)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    };
    if let Err(error) = install() {
        remove(owner).await;
        return Err(error);
    }
    Ok(())
}

/// Match the entire owner tag in the save format (--comment "naut:<id>").
/// Only the chains used by this module are eligible for deletion.
fn deletion_args<'a>(line: &'a str, should_remove: &impl Fn(&str) -> bool) -> Option<Vec<&'a str>> {
    let mut args: Vec<&str> = line.strip_prefix("-A ")?.split_ascii_whitespace().collect();
    if !matches!(
        args.first().copied(),
        Some(CHAIN | CHAIN_OUT | "POSTROUTING")
    ) {
        return None;
    }
    let pos = args.iter().position(|arg| *arg == "--comment")? + 1;
    let tag = args.get(pos)?.trim_matches('"');
    if !should_remove(tag.strip_prefix(COMMENT_PREFIX)?) {
        return None;
    }
    args[pos] = tag;
    args.insert(0, "-D");
    Some(args)
}

fn remove_rules(should_remove: impl Fn(&str) -> bool) {
    let out = match Command::new("iptables-save").args(["-t", "nat"]).output() {
        Ok(o) if o.status.success() => o,
        _ => return,
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(args) = deletion_args(line, &should_remove) {
            if let Err(e) = run_ipt(&args) {
                tracing::warn!(error = %e, "could not remove published port rule");
            }
        }
    }
}

pub async fn remove(owner: &str) {
    stop_forwarders(owner).await;
    remove_rules(|id| id == owner);
}

/// Only this database's confirmed terminal owners are eligible. Host-local
/// leases, interfaces and namespace directories can belong to other runtimes,
/// even when this instance has no running containers. CNI DEL owns those.
pub async fn sweep_network(state: &crate::core::state::StateManager) -> Result<()> {
    let finished = finished_port_owners(state).await?;
    if !finished.is_empty() {
        remove_rules(|id| finished.contains(id));
    }
    Ok(())
}

async fn finished_port_owners(state: &crate::core::state::StateManager) -> Result<HashSet<String>> {
    use crate::core::state::{ContainerStatus, PodStatus};

    let mut finished = HashSet::new();
    for pod in state.list_pods().await? {
        if matches!(pod.status, PodStatus::Succeeded | PodStatus::Failed) {
            finished.insert(pod.id);
        }
    }
    for container in state.list_containers().await? {
        if container.status == ContainerStatus::Stopped {
            finished.insert(container.id);
        }
    }
    Ok(finished)
}

// Publish specs are immutable for an owner. Keep the listeners and their
// connections alive until that pod or container releases the port.
static FORWARDERS: LazyLock<Mutex<HashMap<String, JoinSet<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

async fn start_forwarders(pod_ip: &str, owner: &str, maps: &[PortMapping]) -> Result<()> {
    let mut owners = FORWARDERS.lock().await;
    if owners.contains_key(owner) {
        return Ok(());
    }

    // Bind all ports before spawning, so a conflict drops every partial bind.
    let mut listeners = Vec::new();
    for map in maps.iter().filter(|m| m.uses_loopback_forwarder()) {
        let address = (std::net::Ipv4Addr::LOCALHOST, map.host_port);
        let listener = TcpListener::bind(address).await.map_err(|error| {
            Error::Runtime(format!(
                "bind published port 127.0.0.1:{}: {error}",
                map.host_port
            ))
        })?;
        listeners.push((listener, map.ctr_port));
    }
    if !listeners.is_empty() {
        let mut tasks = JoinSet::new();
        for (listener, port) in listeners {
            tasks.spawn(forward_connections(listener, pod_ip.to_owned(), port));
        }
        owners.insert(owner.to_owned(), tasks);
    }
    Ok(())
}

async fn forward_connections(listener: TcpListener, pod_ip: String, port: u16) {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((mut downstream, _)) = accepted else { break };
                let ip = pod_ip.clone();
                clients.spawn(async move {
                    if let Ok(mut upstream) = TcpStream::connect((ip.as_str(), port)).await {
                        let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
                    }
                });
            }
            _ = clients.join_next(), if !clients.is_empty() => {}
        }
    }
}

async fn stop_forwarders(owner: &str) {
    let mut owners = FORWARDERS.lock().await;
    if let Some(mut tasks) = owners.remove(owner) {
        // Waiting releases the listening sockets before a restart can bind again.
        tasks.shutdown().await;
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use crate::core::state::{PodState, StateManager};
    use serde_json::json;

    #[tokio::test]
    async fn clean_only_terminal_owners_from_this_database() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateManager::new(tmp.path()).unwrap();
        for (id, status) in [("active", "Running"), ("finished", "Succeeded")] {
            let pod: PodState = serde_json::from_value(json!({
                "id": id, "namespace": "test", "name": id, "uid": id, "status": status,
                "created_at": "2026-01-01T00:00:00Z", "containers": [],
                "labels": {}, "annotations": {}
            }))
            .unwrap();
            state.save_pod_state(&pod).await.unwrap();
        }
        let finished = finished_port_owners(&state).await.unwrap();
        let remove = |id: &str| finished.contains(id);
        for id in ["active", "another-instance", "finished-other"] {
            let rule = format!("-A {CHAIN} -m comment --comment \"naut:{id}\" -j DNAT");
            assert!(deletion_args(&rule, &remove).is_none());
        }
        let rule = format!("-A {CHAIN} -m comment --comment \"naut:finished\" -j DNAT");
        assert_eq!(
            deletion_args(&rule, &remove).unwrap(),
            [
                "-D",
                CHAIN,
                "-m",
                "comment",
                "--comment",
                "naut:finished",
                "-j",
                "DNAT"
            ],
        );
        assert!(deletion_args(
            "-A CNI -m comment --comment naut:finished -j RETURN",
            &remove
        )
        .is_none());
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::*;

    #[test]
    fn fresh_node_creates_both_chains_before_installing_their_hooks() {
        let mut chains = HashSet::new();
        let mut hooks = HashSet::new();
        for _ in 0..2 {
            ensure_chains(|args| {
                let success = match args {
                    ["-S", name] => chains.contains(*name),
                    ["-N", name] => chains.insert(name.to_string()),
                    ["-C", source, "-j", target] => {
                        hooks.contains(&(source.to_string(), target.to_string()))
                    }
                    ["-A", source, "-j", target] => {
                        chains.contains(*target)
                            && hooks.insert((source.to_string(), target.to_string()))
                    }
                    _ => panic!("unexpected iptables command {args:?}"),
                };
                if success {
                    Ok(())
                } else {
                    Err(Error::Runtime(format!("failed: {args:?}")))
                }
            })
            .unwrap();
        }
        assert!(hooks.contains(&("PREROUTING".into(), CHAIN.into())));
        assert!(hooks.contains(&("OUTPUT".into(), CHAIN_OUT.into())));
    }

    #[test]
    fn dnat_covers_incoming_udp_and_respects_the_requested_host_ip() {
        let mapping = PortMapping::parse("192.0.2.4:8080:80/udp").unwrap();
        let mut rules = Vec::new();
        mapping
            .install("10.0.0.2", "naut:owner", |args| {
                rules.push(args.join(" "));
                Ok(())
            })
            .unwrap();
        assert!(rules[0].starts_with("-A NAUTILUS-PUB -d 192.0.2.4 -p udp "));
        assert!(rules[1].starts_with("-A NAUTILUS-PUB-OUT -d 192.0.2.4 -p udp "));
        assert!(rules[2].contains("--dport 80"));
        assert!(!mapping.uses_loopback_forwarder());
    }

    #[test]
    fn wildcard_tcp_leaves_localhost_to_the_forwarder() {
        let mapping = PortMapping::parse("0.0.0.0:8080:80").unwrap();
        let mut rules = Vec::new();
        mapping
            .install("10.0.0.2", "naut:owner", |args| {
                rules.push(args.join(" "));
                Ok(())
            })
            .unwrap();
        assert!(rules[0].starts_with("-A NAUTILUS-PUB -p tcp "));
        assert!(rules[1].starts_with("-A NAUTILUS-PUB-OUT ! -d 127.0.0.1 -p tcp "));
        assert!(mapping.uses_loopback_forwarder());
    }
}

#[cfg(test)]
mod forwarder_tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    async fn unused_port() -> u16 {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[tokio::test]
    async fn repeat_publish_then_stop_releases_port_and_active_connection() {
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let host_port = unused_port().await;
        let maps = [PortMapping::parse(&format!(
            "{host_port}:{}",
            target.local_addr().unwrap().port()
        ))
        .unwrap()];
        let addr = (Ipv4Addr::LOCALHOST, host_port);
        start_forwarders("127.0.0.1", "pod-a", &maps).await.unwrap();
        start_forwarders("127.0.0.1", "pod-a", &maps).await.unwrap();
        assert!(start_forwarders("127.0.0.1", "pod-b", &maps).await.is_err());
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"x").await.unwrap();
        let timeout_limit = Duration::from_secs(5);
        let (mut backend, _) = timeout(timeout_limit, target.accept())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            timeout(timeout_limit, backend.read_u8())
                .await
                .unwrap()
                .unwrap(),
            b'x'
        );

        stop_forwarders("pod-a").await;
        start_forwarders("127.0.0.1", "pod-b", &maps).await.unwrap();
        assert_eq!(
            timeout(timeout_limit, client.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        stop_forwarders("pod-b").await;
    }

    #[tokio::test]
    async fn later_port_conflict_releases_earlier_bindings() {
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let first = unused_port().await;
        let second = occupied.local_addr().unwrap().port();
        let maps = [
            PortMapping::parse(&format!("127.0.0.1:{first}:80")).unwrap(),
            PortMapping::parse(&format!("{second}:80")).unwrap(),
        ];
        assert!(start_forwarders("127.0.0.1", "multi-port", &maps)
            .await
            .is_err());
        TcpListener::bind((Ipv4Addr::LOCALHOST, first))
            .await
            .unwrap();
    }
}
