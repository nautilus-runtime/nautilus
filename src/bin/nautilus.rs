//! nautilus — docker-style client for the nautilusd CRI runtime.
//!
//! Talks CRI gRPC over the daemon's unix socket and maps docker semantics onto
//! it: each `run` creates an implicit single-container sandbox (labeled
//! `nautilus.ctl=true`) that `rm` tears down again.
//!
//! Socket selection order: --socket flag > $NAUTILUS_SOCKET > /run/nautilus/nautilus.sock
//! The socket is root-owned by default; run under sudo / a group with access.

use clap::{Parser, Subcommand};
use nautilus::proto::runtime::v1::{
    image_service_client::ImageServiceClient,
    runtime_service_client::RuntimeServiceClient,
    ContainerConfig, ContainerMetadata, ContainerState, ContainerStatsRequest,
    ContainerStatusRequest, ContainerStatusResponse, CreateContainerRequest,
    CreateContainerResponse, ExecSyncRequest, ImageSpec, ImageStatusRequest, KeyValue,
    LinuxContainerConfig, LinuxContainerResources, ListContainerStatsRequest,
    ListContainersRequest, ListImagesRequest, PullImageRequest,
    RemoveContainerRequest, RemoveImageRequest, RemovePodSandboxRequest, RunPodSandboxRequest,
    StartContainerRequest, StatusRequest, StopContainerRequest, ImageFsInfoRequest,
    ContainerFilter, PodSandboxConfig, PodSandboxMetadata, PodSandboxStatusRequest, VersionRequest,
};
use hyper_util::rt::TokioIo;
use std::process::ExitCode;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::{Request, Status};
use tower::service_fn;

const DEFAULT_SOCKET: &str = "/run/nautilus/nautilus.sock";
/// Marker label on every sandbox/ctl container so tooling can tell them apart.
const CTL_LABEL: &str = "nautilus.ctl";

#[derive(Parser)]
#[command(
    name = "nautilus",
    version = nautilus::VERSION,
    about = "Docker-style client for the nautilus container runtime",
    disable_help_subcommand = true
)]
struct Cli {
    /// Runtime gRPC unix socket (default: $NAUTILUS_SOCKET or /run/nautilus/nautilus.sock)
    #[arg(short, long, global = true)]
    socket: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a command in a new container (defaults to detached, like `docker run -d`)
    Run {
        /// Container name (generated when omitted)
        #[arg(long)]
        name: Option<String>,
        /// Environment variables (KEY=VALUE, repeatable)
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Labels (KEY=VALUE, repeatable)
        #[arg(short = 'l', long = "label")]
        label: Vec<String>,
        /// Publish container port: [hostIp:]hostPort:containerPort[/proto] (repeatable)
        #[arg(short = 'p', long = "publish")]
        publish: Vec<String>,
        /// Memory limit (e.g. 32m, 1g, or plain bytes)
        #[arg(long)]
        memory: Option<String>,
        /// CPU limit in cores (e.g. 0.5, 1.5)
        #[arg(long)]
        cpus: Option<f64>,
        /// CPUs in which to allow execution (e.g. "0", "0-2")
        #[arg(long)]
        cpuset_cpus: Option<String>,
        /// Sandbox runtime handler: runc | wasi | wasm
        #[arg(long, default_value = "runc")]
        runtime: String,
        /// Override the image entrypoint
        #[arg(long)]
        entrypoint: Option<String>,
        /// Working directory inside the container
        #[arg(short = 'w', long = "workdir")]
        workdir: Option<String>,
        /// Wait for exit, print captured output, then remove (foreground-like)
        #[arg(long)]
        rm: bool,
        /// Image reference (OCI image; wasm modules use a file:// path instead)
        image: String,
        /// Command + arguments (overrides the image CMD; entrypoint kept unless --entrypoint)
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// List containers (-a to include exited ones)
    Ps {
        #[arg(short = 'a', long = "all")]
        all: bool,
        #[arg(short = 'n', long = "last", default_value_t = 0)]
        last: usize,
    },
    /// List images
    Images {
        /// Only show images whose repository:tag contains this string
        repo: Option<String>,
    },
    /// Pull an image from a registry
    Pull { image: String },
    /// Remove an image
    Rmi { image: String },
    /// Start one or more stopped containers
    Start { targets: Vec<String> },
    /// Stop one or more running containers
    Stop { targets: Vec<String> },
    /// Restart one or more containers
    Restart { targets: Vec<String> },
    /// Force-stop one or more containers
    Kill { targets: Vec<String> },
    /// Remove one or more containers (and their implicit sandboxes)
    Rm {
        /// Remove running containers (stop them first)
        #[arg(short = 'f')]
        force: bool,
        targets: Vec<String>,
    },
    /// Remove all exited containers and their implicit sandboxes
    Prune,
    /// Fetch container logs (captured stdout+stderr)
    Logs {
        /// Follow output
        #[arg(short = 'f')]
        follow: bool,
        /// Number of trailing lines to show (0 = all)
        #[arg(long, default_value_t = 0)]
        tail: usize,
        target: String,
    },
    /// Run a command in a running container (exit code is propagated)
    Exec {
        target: String,
        /// Command and arguments
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Inspect a container (JSON status document)
    Inspect { target: String },
    /// Show live resource usage of containers
    Stats {
        /// Restrict to one container
        target: Option<String>,
    },
    /// Show runtime status
    Info,
    /// Show client/runtime version information
    Version,
}

fn main() -> ExitCode {
    // Piping into `head` closes stdout early; without this the default
    // SIGPIPE-ignored Rust behaviour turns EPIPE into a println! panic.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = match rt.block_on(dispatch(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {}", e);
            1
        }
    };
    ExitCode::from(code)
}

type RClient = RuntimeServiceClient<Channel>;
type IClient = ImageServiceClient<Channel>;

async fn connect(socket: &str) -> Result<(RClient, IClient), String> {
    let p = socket.to_string();
    let channel: Channel = Endpoint::try_from("http://nautilus")
        .map_err(|e| format!("invalid endpoint: {e}"))?
        .connect_with_connector_lazy(service_fn(move |_: Uri| {
            let path = p.clone();
            async move {
                UnixStream::connect(path)
                    .await
                    .map(TokioIo::new)
                    .map_err(std::io::Error::other)
            }
        }));
    Ok((
        RuntimeServiceClient::new(channel.clone()),
        ImageServiceClient::new(channel),
    ))
}

async fn dispatch(cli: Cli) -> Result<u8, String> {
    let socket = cli
        .socket
        .or_else(|| std::env::var("NAUTILUS_SOCKET").ok())
        .unwrap_or_else(|| DEFAULT_SOCKET.to_string());
    let (mut r, mut i) = connect(&socket).await?;

    match cli.command {
        Commands::Exec { target, command } => return cmd_exec(&mut r, target, command).await,
        Commands::Run {
            name,
            env,
            label,
            publish,
            memory,
            cpus,
            cpuset_cpus,
            runtime,
            entrypoint,
            workdir,
            rm,
            image,
            command,
        } => {
            cmd_run(
                &mut r, &mut i, name, env, label, publish, memory, cpus, cpuset_cpus,
                runtime, entrypoint, workdir, rm, image, command,
            )
            .await?
        }
        Commands::Ps { all, last } => cmd_ps(&mut r, all, last).await?,
        Commands::Images { repo } => cmd_images(&mut i, repo).await?,
        Commands::Pull { image } => cmd_pull(&mut i, image).await?,
        Commands::Rmi { image } => cmd_rmi(&mut i, image).await?,
        Commands::Start { targets } => cmd_lifecycle(&mut r, targets, Action::Start).await?,
        Commands::Stop { targets } => cmd_lifecycle(&mut r, targets, Action::Stop).await?,
        Commands::Restart { targets } => cmd_lifecycle(&mut r, targets, Action::Restart).await?,
        Commands::Kill { targets } => cmd_lifecycle(&mut r, targets, Action::Kill).await?,
        Commands::Rm { force, targets } => cmd_rm(&mut r, targets, force).await?,
        Commands::Prune => cmd_prune(&mut r).await?,
        Commands::Logs { follow, tail, target } => cmd_logs(&mut r, target, follow, tail).await?,
        Commands::Inspect { target } => cmd_inspect(&mut r, target).await?,
        Commands::Stats { target } => cmd_stats(&mut r, target).await?,
        Commands::Info => cmd_info(&mut r, &mut i).await?,
        Commands::Version => cmd_version(&mut r).await?,
    }
    Ok(0)
}

// ---------------------------------------------------------------- helpers --

fn kv_pairs(items: Vec<String>, what: &str) -> Result<Vec<(String, String)>, String> {
    items
        .into_iter()
        .map(|s| match s.split_once('=') {
            Some((k, v)) => Ok((k.to_string(), v.to_string())),
            None => Err(format!("invalid {} (want KEY=VALUE): {}", what, s)),
        })
        .collect()
}

fn parse_memory(s: &str) -> Result<i64, String> {
    let t = s.trim().to_lowercase();
    let (num, mult) = match t.chars().last() {
        Some('k') if t.len() > 1 => (&t[..t.len() - 1], 1024i64),
        Some('m') if t.len() > 1 => (&t[..t.len() - 1], 1024 * 1024),
        Some('g') if t.len() > 1 => (&t[..t.len() - 1], 1024 * 1024 * 1024),
        _ => (&t[..t.len()], 1),
    };
    num.trim()
        .parse::<i64>()
        .map(|n| n * mult)
        .map_err(|_| format!("invalid memory size: {}", s))
}

fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

fn ago(nanos: i64) -> String {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(nanos / 1_000_000_000, 0)
        .unwrap_or_default();
    let secs = (chrono::Utc::now() - dt).num_seconds().max(0);
    match secs {
        s if s < 60 => format!("{} second{} ago", s, plural(s)),
        m if m < 3600 => format!("{} minute{} ago", m / 60, plural(m / 60)),
        h if h < 86400 => format!("{} hour{} ago", h / 3600, plural(h / 3600)),
        d if d < 604800 => format!("{} day{} ago", d / 86400, plural(d / 86400)),
        w if w < 2592000 => format!("{} week{} ago", w / 604800, plural(w / 604800)),
        mo if mo < 31536000 => format!("{} month{} ago", mo / 2592000, plural(mo / 2592000)),
        y => format!("{} year{} ago", y / 31536000, plural(y / 31536000)),
    }
}

fn plural(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{}B", bytes)
    } else {
        format!("{:.1}{}", v, UNITS[u])
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

fn status(s: Status) -> String {
    format!("daemon error ({}): {}", s.code(), s.message())
}

/// Resolve a user-supplied target (id / id-prefix / name) against the daemon.
async fn resolve_container(
    r: &mut RClient,
    target: &str,
) -> Result<nautilus::proto::runtime::v1::Container, String> {
    let resp = r
        .list_containers(Request::new(ListContainersRequest::default()))
        .await
        .map_err(status)?
        .into_inner();
    let list = resp.containers;
    // Exact id first.
    if let Some(c) = list.iter().find(|c| c.id == target) {
        return Ok(c.clone());
    }
    // Then exact name.
    let by_name: Vec<_> = list
        .iter()
        .filter(|c| c.metadata.as_ref().map(|m| m.name.as_str()) == Some(target))
        .collect();
    match by_name.len() {
        1 => return Ok(by_name[0].clone()),
        n if n > 1 => return Err(format!("ambiguous container name: {}", target)),
        _ => {}
    }
    // Then unique id prefix.
    let by_prefix: Vec<_> = list.iter().filter(|c| c.id.starts_with(target)).collect();
    match by_prefix.len() {
        1 => Ok(by_prefix[0].clone()),
        0 => Err(format!("No such object: {}", target)),
        _ => Err(format!("ambiguous container id prefix: {}", target)),
    }
}

fn state_string(state: i32) -> &'static str {
    match ContainerState::try_from(state) {
        Ok(ContainerState::ContainerCreated) => "Created",
        Ok(ContainerState::ContainerRunning) => "Running",
        Ok(ContainerState::ContainerExited) => "Exited",
        _ => "Unknown",
    }
}

async fn container_status(r: &mut RClient, id: &str) -> Result<ContainerStatusResponse, String> {
    let resp = r
        .container_status(Request::new(ContainerStatusRequest {
            container_id: id.to_string(),
            verbose: false,
        }))
        .await
        .map_err(status)?
        .into_inner();
    Ok(resp)
}

/// Make sure `image` is usable by the daemon. Returns the reference the caller
/// must use for container creation: normally None (keep as-is), but when the
/// user passed a local image-ID prefix (`nautilus run a952f42cef39`, docker
/// semantics) the canonical repo tag of that image is returned instead.
async fn ensure_image(i: &mut IClient, image: &str) -> Result<Option<String>, String> {
    let st = i
        .image_status(Request::new(ImageStatusRequest {
            image: Some(ImageSpec {
                image: image.to_string(),
                ..Default::default()
            }),
            verbose: false,
        }))
        .await
        .map_err(status)?
        .into_inner();
    if st.image.is_some() {
        return Ok(None);
    }
    // Docker parity: a hex prefix matches a locally-pulled image by its
    // config-digest IMAGE ID — never treat it as a registry name.
    if is_image_id_prefix(image) {
        return match resolve_local_id(i, image).await {
            Ok(Some(resolved)) => {
                eprintln!("Resolved '{}' to local image {}", image, resolved);
                Ok(Some(resolved))
            }
            Ok(None) => Err(format!(
                "No image matching ID prefix '{}' found locally (try `nautilus pull` first)",
                image
            )),
            Err(e) => Err(e),
        };
    }
    eprintln!("Unable to find image '{}' locally; pulling", image);
    pull_image(i, image).await.map(|_| None)
}

/// True when `s` looks like a truncated content digest: 4+ hex chars.
fn is_image_id_prefix(s: &str) -> bool {
    s.len() >= 4 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Find the unique local image whose config-digest ID starts with `prefix`.
/// Ambiguous or missing matches produce docker-style errors.
async fn resolve_local_id(i: &mut IClient, prefix: &str) -> Result<Option<String>, String> {
    let resp = i
        .list_images(Request::new(ListImagesRequest::default()))
        .await
        .map_err(status)?
        .into_inner();
    let mut hits: Vec<String> = Vec::new();
    for img in &resp.images {
        let id = img.nautilus_image_id.trim_start_matches("sha256:");
        if id.starts_with(prefix) {
            hits.push(
                img.repo_tags
                    .first()
                    .cloned()
                    .unwrap_or_else(|| img.id.clone()),
            );
        }
    }
    match hits.len() {
        0 => Ok(None),
        1 => Ok(hits.pop()),
        n => Err(format!(
            "Ambiguous image ID prefix '{}' matches {} images: {}",
            prefix,
            n,
            hits.join(", ")
        )),
    }
}

async fn pull_image(i: &mut IClient, image: &str) -> Result<String, String> {
    let resp = i
        .pull_image(Request::new(PullImageRequest {
            image: Some(ImageSpec {
                image: image.to_string(),
                ..Default::default()
            }),
            auth: None,
            sandbox_config: None,
        }))
        .await
        .map_err(status)?
        .into_inner();
    Ok(resp.image_ref)
}

// ------------------------------------------------------------------- core --

#[allow(clippy::too_many_arguments)]
async fn cmd_run(
    r: &mut RClient,
    i: &mut IClient,
    name: Option<String>,
    env: Vec<String>,
    label: Vec<String>,
    publish: Vec<String>,
    memory: Option<String>,
    cpus: Option<f64>,
    cpuset_cpus: Option<String>,
    runtime: String,
    entrypoint: Option<String>,
    workdir: Option<String>,
    rm: bool,
    image: String,
    command: Vec<String>,
) -> Result<(), String> {
    let env = kv_pairs(env, "env")?;
    let mut labels = kv_pairs(label, "label")?;

    // Docker-style port publishing rides as a normal label so it survives in
    // CRI status output and daemon-side state alike.
    if !publish.is_empty() {
        let maps = publish
            .iter()
            .map(|spec| {
                nautilus::network::publish::PortMapping::parse(spec).map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, String>>()?;
        labels.push((
            nautilus::network::publish::PUBLISH_LABEL.to_string(),
            nautilus::network::publish::PortMapping::encode_all(&maps),
        ));
    }

    let cname = match name {
        Some(n) => n,
        None => format!("ctl-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
    };
    // Name-uniqueness check client-side (names are metadata only server-side).
    {
        let resp = r
            .list_containers(Request::new(ListContainersRequest::default()))
            .await
            .map_err(status)?;
        if resp
            .into_inner()
            .containers
            .iter()
            .any(|c| c.metadata.as_ref().map(|m| m.name.as_str()) == Some(cname.as_str()))
        {
            return Err(format!(
                "Conflict: container name \"{}\" already in use",
                cname
            ));
        }
    }

    // wasm/wasi workloads reference local file:// modules — no registry pull.
    let mut image = image;
    if runtime.is_empty() || runtime == "runc" {
        if let Some(resolved) = ensure_image(i, &image).await? {
            image = resolved;
        }
    }

    let memory_limit = memory.as_deref().map(parse_memory).transpose()?;

    // Implicit single-container sandbox.
    let uid = uuid::Uuid::new_v4().to_string();
    let pod = PodSandboxConfig {
        metadata: Some(PodSandboxMetadata {
            name: cname.clone(),
            uid,
            namespace: "ctl".to_string(),
            attempt: 0,
        }),
        labels: [(CTL_LABEL.to_string(), "true".to_string())].into_iter().collect(),
        ..Default::default()
    };
    // Empty handler selects the daemon default ("runc").
    let handler = if runtime == "runc" { String::new() } else { runtime.clone() };
    let pod_id = r
        .run_pod_sandbox(Request::new(RunPodSandboxRequest {
            config: Some(pod),
            runtime_handler: handler,
        }))
        .await
        .map_err(status)?
        .into_inner()
        .pod_sandbox_id;

    let resources = LinuxContainerResources {
        cpu_period: if cpus.is_some() { 100_000 } else { 0 },
        cpu_quota: cpus.map(|c| (c * 100_000f64) as i64).unwrap_or(0),
        memory_limit_in_bytes: memory_limit.unwrap_or(0),
        cpuset_cpus: cpuset_cpus.unwrap_or_default(),
        ..Default::default()
    };

    let ctr_config = ContainerConfig {
        metadata: Some(ContainerMetadata {
            name: cname.clone(),
            attempt: 0,
        }),
        image: Some(ImageSpec {
            image: image.clone(),
            ..Default::default()
        }),
        command: entrypoint.map(|e| vec![e]).unwrap_or_default(),
        args: command,
        envs: env
            .into_iter()
            .map(|(k, v)| KeyValue {
                key: k,
                value: v.into_bytes(),
            })
            .collect(),
        working_dir: workdir.unwrap_or_default(),
        labels: labels.into_iter().collect(),
        linux: Some(LinuxContainerConfig {
            resources: Some(resources),
            ..Default::default()
        }),
        ..Default::default()
    };

    let mut created_id = None;
    let workload: Result<(), String> = async {
        let response: CreateContainerResponse = r.create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: pod_id.clone(),
            config: Some(ctr_config),
            sandbox_config: None,
        })).await.map_err(status)?.into_inner();
        let id = response.container_id;
        if id.is_empty() {
            return Err("daemon returned an empty container id".into());
        }
        created_id = Some(id.clone());
        r.start_container(Request::new(StartContainerRequest { container_id: id.clone() }))
            .await.map_err(status)?;
        println!("{id}");
        if rm {
            wait_exit(r, &id).await?;
            // A batch backend may finish without creating a log file.
            let _ = print_log_tail(r, &id, 0).await;
        }
        Ok(())
    }.await;
    if rm || workload.is_err() {
        let condition = if workload.is_ok() { Some("exited") } else { Some("not-running") };
        let cleanup = remove_implicit_workload(r, created_id.as_deref(), &pod_id, condition).await.map(|_| ());
        return match (workload, cleanup) {
            (Err(error), Err(cleanup)) => Err(format!("{error}; cleanup of sandbox {pod_id} failed: {cleanup}")),
            (result, Ok(())) => result,
            (Ok(()), error) => error,
        };
    }
    Ok(())
}

async fn wait_exit(r: &mut RClient, id: &str) -> Result<(), String> {
    loop {
        let exited = container_status(r, id)
            .await?
            .status
            .map(|s| s.state == ContainerState::ContainerExited as i32)
            .unwrap_or(true);
        if exited {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

async fn cmd_ps(r: &mut RClient, all: bool, last: usize) -> Result<(), String> {
    let resp = r
        .list_containers(Request::new(ListContainersRequest::default()))
        .await
        .map_err(status)?
        .into_inner();

    let ports_of = |labels: &std::collections::HashMap<String, String>| -> String {
        labels
            .get(nautilus::network::publish::PUBLISH_LABEL)
            .map(|spec| {
                nautilus::network::publish::PortMapping::parse_list(spec)
                    .iter()
                    .map(|m| m.display())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };

    // Collect first so -n can show the most recently created N.
    let mut rows: Vec<(String, String, String, String, String, i64)> = Vec::new();
    for c in &resp.containers {
        if !all && c.state != ContainerState::ContainerRunning as i32 {
            continue;
        }
        let name = c
            .metadata
            .as_ref()
            .map(|m| m.name.clone())
            .unwrap_or_default();
        let status_txt = if c.state == ContainerState::ContainerExited as i32 {
            let code = container_status(r, &c.id)
                .await
                .ok()
                .and_then(|s| s.status)
                .map(|s| s.exit_code);
            match code {
                Some(cd) => format!("Exited ({})", cd),
                None => "Exited".to_string(),
            }
        } else {
            state_string(c.state).to_string()
        };
        rows.push((
            short(&c.id),
            truncate(&c.image_ref, 29),
            status_txt,
            ports_of(&c.labels),
            truncate(&name, 19),
            c.created_at,
        ));
    }    if last > 0 && rows.len() > last {
        rows.sort_by(|a, b| b.5.cmp(&a.5));
        rows.truncate(last);
    }

    println!(
        "{:<14} {:<30} {:<16} {:<26} {}",
        "CONTAINER ID", "IMAGE", "STATUS", "PORTS", "NAMES"
    );
    for row in rows {
        println!(
            "{:<14} {:<30} {:<16} {:<26} {}",
            row.0, row.1, row.2, row.3, row.4
        );
    }
    Ok(())
}

async fn cmd_images(i: &mut IClient, repo: Option<String>) -> Result<(), String> {
    let resp = i
        .list_images(Request::new(ListImagesRequest { filter: None }))
        .await
        .map_err(status)?
        .into_inner();

    // docker layout: REPOSITORY TAG IMAGE ID CREATED SIZE
    println!(
        "{:<32} {:<12} {:<20} {:<16} {}",
        "REPOSITORY", "TAG", "IMAGE ID", "CREATED", "SIZE"
    );
    let mut shown = 0usize;
    for img in &resp.images {
        let tag_full = img
            .repo_tags
            .first()
            .cloned()
            .unwrap_or_else(|| "<none>:<none>".into());
        let (img_repo, img_tag) = match tag_full.rsplit_once(':') {
            Some((r, t)) if !t.contains('/') => (r.to_string(), t.to_string()),
            _ => (tag_full.clone(), "<none>".to_string()),
        };
        if let Some(pat) = &repo {
            if !img_repo.contains(pat.as_str()) && !img_tag.contains(pat.as_str()) {
                continue;
            }
        }
        let image_id = {
            let raw = if img.nautilus_image_id.is_empty() {
                &img.id
            } else {
                &img.nautilus_image_id
            };
            let hex = raw.trim_start_matches("sha256:");
            String::from_utf8_lossy(&hex.as_bytes()[..hex.len().min(12)]).to_string()
        };
        let created_nanos = if img.nautilus_created_at > 0 {
            img.nautilus_created_at * 1_000_000_000
        } else {
            0
        };
        println!(
            "{:<32} {:<12} {:<20} {:<16} {}",
            truncate(&img_repo, 31),
            truncate(&img_tag, 11),
            image_id,
            if created_nanos > 0 {
                ago(created_nanos)
            } else {
                "?".to_string()
            },
            human_size(img.size)
        );
        shown += 1;
    }
    let _ = shown;
    Ok(())
}


async fn cmd_pull(i: &mut IClient, image: String) -> Result<(), String> {
    eprintln!("Pulling {} ...", image);
    let reference = pull_image(i, &image).await?;
    eprintln!("Done.");
    println!("{}", reference);
    Ok(())
}

async fn cmd_rmi(i: &mut IClient, image: String) -> Result<(), String> {
    i.remove_image(Request::new(RemoveImageRequest {
        image: Some(ImageSpec {
            image: image.clone(),
            ..Default::default()
        }),
    }))
    .await
    .map_err(status)?;
    println!("Removed: {}", image);
    Ok(())
}

enum Action {
    Start,
    Stop,
    Restart,
    Kill,
}

async fn cmd_lifecycle(r: &mut RClient, targets: Vec<String>, action: Action) -> Result<(), String> {
    if targets.is_empty() {
        return Err("requires at least 1 container".into());
    }
    for t in &targets {
        let c = resolve_container(r, t).await?;
        let res: Result<(), Status> = match action {
            Action::Start => r
                .start_container(Request::new(StartContainerRequest {
                    container_id: c.id.clone(),
                }))
                .await
                .map(|_| ()),
            Action::Stop | Action::Kill => r
                .stop_container(Request::new(StopContainerRequest {
                    container_id: c.id.clone(),
                    timeout: 0,
                }))
                .await
                .map(|_| ()),
            Action::Restart => {
                let stop_res = r
                    .stop_container(Request::new(StopContainerRequest {
                        container_id: c.id.clone(),
                        timeout: 0,
                    }))
                    .await;
                if let Err(e) = stop_res {
                    eprintln!("Error stopping {}: {}", t, status(e));
                    continue;
                }
                r.start_container(Request::new(StartContainerRequest {
                    container_id: c.id.clone(),
                }))
                .await
                .map(|_| ())
            }
        };
        match res {
            Ok(_) => println!("{}", c.id),
            Err(e) => eprintln!("Error response from daemon for {}: {}", t, status(e)),
        }
    }
    Ok(())
}

async fn remove_implicit_workload(
    r: &mut RClient,
    id: Option<&str>,
    pod_id: &str,
    condition: Option<&'static str>,
) -> Result<bool, String> {
    if let Some(id) = id {
        let mut request = Request::new(RemoveContainerRequest { container_id: id.to_string() });
        if let Some(value) = condition {
            request.metadata_mut().insert("nautilus-remove-if", tonic::metadata::MetadataValue::from_static(value));
        }
        match r.remove_container(request).await {
            Err(error) if condition == Some("exited") && error.code() == tonic::Code::FailedPrecondition => return Ok(false),
            result => { result.map_err(status)?; }
        }
    }
    if pod_id.is_empty() {
        return Ok(true);
    }
    let request = PodSandboxStatusRequest { pod_sandbox_id: pod_id.to_string(), ..Default::default() };
    let pod = match r.pod_sandbox_status(Request::new(request)).await {
        Ok(response) => response.into_inner().status,
        Err(error) if error.code() == tonic::Code::NotFound => return Ok(true),
        Err(error) => return Err(status(error)),
    };
    let Some(pod) = pod else { return Err(format!("missing status for sandbox {pod_id}")); };
    if !pod.metadata.as_ref().is_some_and(|m| m.namespace == "ctl")
        || pod.labels.get(CTL_LABEL).map(String::as_str) != Some("true")
    {
        return Ok(true);
    }
    let remaining = r.list_containers(Request::new(ListContainersRequest {
        filter: Some(ContainerFilter { pod_sandbox_id: pod_id.to_string(), ..Default::default() }),
    })).await.map_err(status)?.into_inner().containers;
    if !remaining.is_empty() {
        return Ok(true);
    }
    // The daemon repeats the ownership and emptiness checks under its create/remove lock.
    let mut request = Request::new(RemovePodSandboxRequest { pod_sandbox_id: pod_id.to_string() });
    request.metadata_mut().insert("nautilus-if-implicit-empty", "true".parse().unwrap());
    r.remove_pod_sandbox(request).await.map_err(status)?;
    Ok(true)
}

async fn cmd_rm(r: &mut RClient, targets: Vec<String>, force: bool) -> Result<(), String> {
    if targets.is_empty() {
        return Err("requires at least 1 container".into());
    }
    for t in &targets {
        let c = resolve_container(r, t).await?;
        if c.state == ContainerState::ContainerRunning as i32 {
            if !force {
                return Err(format!(
                    "cannot remove container {}: container is running; use -f",
                    short(&c.id)
                ));
            }
        }
        let condition = if force { None } else { Some("not-running") };
        remove_implicit_workload(r, Some(&c.id), &c.pod_sandbox_id, condition).await?;
        println!("{}", c.id);
    }
    Ok(())
}

async fn cmd_prune(r: &mut RClient) -> Result<(), String> {
    let resp = r
        .list_containers(Request::new(ListContainersRequest::default()))
        .await
        .map_err(status)?
        .into_inner();
    let exited: Vec<_> = resp
        .containers
        .iter()
        .filter(|c| c.state == ContainerState::ContainerExited as i32)
        .collect();
    let mut removed = 0;
    for c in &exited {
        if remove_implicit_workload(r, Some(&c.id), &c.pod_sandbox_id, Some("exited")).await? {
            println!("{}", c.id);
            removed += 1;
        }
    }
    eprintln!("Total reclaimed containers: {removed}");
    Ok(())
}

async fn read_log(path: &str, offset: u64) -> Result<Vec<u8>, String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("log not readable (started? permissions?): {}: {}", path, e))?;
    let len = f.metadata().await.map_err(|e| e.to_string())?.len();
    if len <= offset {
        return Ok(Vec::new());
    }
    f.seek(std::io::SeekFrom::Start(offset)).await.map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).await.map_err(|e| e.to_string())?;
    Ok(buf)
}

async fn log_path_of(r: &mut RClient, id: &str) -> Result<String, String> {
    let lp = container_status(r, id)
        .await?
        .status
        .map(|s| s.log_path)
        .unwrap_or_default();
    if lp.is_empty() {
        Err("log path unknown for this container".to_string())
    } else {
        Ok(lp)
    }
}

async fn print_log_tail(r: &mut RClient, id: &str, tail: usize) -> Result<(), String> {
    let path = log_path_of(r, id).await?;
    let data = read_log(&path, 0).await?;
    let text = String::from_utf8_lossy(&data);
    let lines: Vec<&str> = text.lines().collect();
    let start = if tail > 0 { lines.len().saturating_sub(tail) } else { 0 };
    for l in &lines[start..] {
        println!("{}", l);
    }
    Ok(())
}

async fn cmd_logs(r: &mut RClient, target: String, follow: bool, tail: usize) -> Result<(), String> {
    let c = resolve_container(r, &target).await?;
    let path = log_path_of(r, &c.id).await?;

    if !follow {
        let data = read_log(&path, 0).await?;
        let text = String::from_utf8_lossy(&data);
        let lines: Vec<&str> = text.lines().collect();
        let start = if tail > 0 { lines.len().saturating_sub(tail) } else { 0 };
        for l in &lines[start..] {
            println!("{}", l);
        }
        return Ok(());
    }

    // Follow: incremental reads until the container exits and output drains.
    let mut offset: u64 = if tail > 0 {
        let data = read_log(&path, 0).await?;
        let text = String::from_utf8_lossy(&data);
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(tail);
        let skip: u64 = lines[..start]
            .iter()
            .map(|l| l.len() as u64 + 1)
            .sum();
        skip.min(data.len() as u64)
    } else {
        0
    };
    loop {
        let data = read_log(&path, offset).await?;
        if !data.is_empty() {
            offset += data.len() as u64;
            write_stdout(&data);
        }
        let exited = container_status(r, &c.id)
            .await
            .ok()
            .and_then(|s| s.status)
            .map(|s| s.state == ContainerState::ContainerExited as i32)
            .unwrap_or(true);
        if exited {
            let rest = read_log(&path, offset).await?;
            if !rest.is_empty() {
                write_stdout(&rest);
            }
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Ok(())
}

fn write_stdout(bytes: &[u8]) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

async fn cmd_exec(r: &mut RClient, target: String, command: Vec<String>) -> Result<u8, String> {
    if command.is_empty() {
        return Err("exec requires a command".into());
    }
    let c = resolve_container(r, &target).await?;
    let resp = r
        .exec_sync(Request::new(ExecSyncRequest {
            container_id: c.id,
            cmd: command,
            timeout: 0,
        }))
        .await
        .map_err(status)?
        .into_inner();
    write_stdout(&resp.stdout);
    {
        use std::io::Write;
        let mut err = std::io::stderr();
        let _ = err.write_all(&resp.stderr);
        let _ = err.flush();
    }
    if resp.exit_code < 0 {
        Ok(125)
    } else {
        Ok(resp.exit_code as u8)
    }
}

async fn cmd_inspect(r: &mut RClient, target: String) -> Result<(), String> {
    use nautilus::proto::runtime::v1::{PodSandboxState, PodSandboxStatusRequest};
    let c = resolve_container(r, &target).await?;
    let st = container_status(r, &c.id).await?;
    let s = st.status.ok_or_else(|| format!("No such object: {}", target))?;

    let pod = if c.pod_sandbox_id.is_empty() {
        None
    } else {
        r.pod_sandbox_status(Request::new(PodSandboxStatusRequest {
            pod_sandbox_id: c.pod_sandbox_id.clone(),
            verbose: false,
        }))
        .await
        .ok()
        .and_then(|p| p.into_inner().status)
    };

    let doc = serde_json::json!({
        "Id": s.id,
        "Name": s.metadata.as_ref().map(|m| m.name.clone()).unwrap_or_default(),
        "Image": s.image.as_ref().map(|i| i.image.clone()).unwrap_or_default(),
        "ImageRef": s.image_ref,
        "State": state_string(s.state),
        "ExitCode": s.exit_code,
        "CreatedAt": s.created_at,
        "StartedAt": s.started_at,
        "FinishedAt": s.finished_at,
        "LogPath": s.log_path,
        "Labels": s.labels,
        "Annotations": s.annotations,
        "Publish": s
            .labels
            .get(nautilus::network::publish::PUBLISH_LABEL)
            .map(|spec| {
                nautilus::network::publish::PortMapping::parse_list(spec)
                    .iter()
                    .map(|m| m.display())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        "Sandbox": pod.map(|p| serde_json::json!({
            "Id": c.pod_sandbox_id,
            "State": p.state,
            "Ready": p.state == PodSandboxState::SandboxReady as i32,
            "Ip": p.network.as_ref().map(|n| n.ip.clone()).unwrap_or_default(),
            "RuntimeHandler": p.runtime_handler,
        })),
    });
    println!("{}", serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?);
    Ok(())
}

async fn cmd_stats(r: &mut RClient, target: Option<String>) -> Result<(), String> {
    println!(
        "{:<14} {:<24} {:>12} {:>10}",
        "CONTAINER ID", "NAME", "MEM USAGE", "CPU %"
    );
    let render = |id: &str, name: &str, mem: u64, cpu_ns: u64| {
        println!(
            "{:<14} {:<24} {:>12} {:>9.2}%",
            short(id),
            truncate(name, 23),
            human_size(mem),
            cpu_ns as f64 / 1e9 * 100.0
        );
    };
    match target {
        Some(t) => {
            let c = resolve_container(r, &t).await?;
            let resp = r
                .container_stats(Request::new(ContainerStatsRequest {
                    container_id: c.id.clone(),
                }))
                .await
                .map_err(status)?
                .into_inner();
            if let Some(s) = resp.stats {
                let mem = s
                    .memory
                    .as_ref()
                    .and_then(|m| m.working_set_bytes.clone())
                    .map(|v| v.value)
                    .unwrap_or(0);
                let cpu = s
                    .cpu
                    .as_ref()
                    .and_then(|c| c.usage_core_nano_seconds.clone())
                    .map(|v| v.value)
                    .unwrap_or(0);
                let name = s
                    .attributes
                    .as_ref()
                    .and_then(|a| a.metadata.clone())
                    .map(|m| m.name)
                    .unwrap_or_default();
                render(&c.id, &name, mem, cpu);
            }
        }
        None => {
            let resp = r
                .list_container_stats(Request::new(ListContainerStatsRequest::default()))
                .await
                .map_err(status)?
                .into_inner();
            for s in resp.stats {
                let mem = s
                    .memory
                    .as_ref()
                    .and_then(|m| m.working_set_bytes.clone())
                    .map(|v| v.value)
                    .unwrap_or(0);
                let cpu = s
                    .cpu
                    .as_ref()
                    .and_then(|c| c.usage_core_nano_seconds.clone())
                    .map(|v| v.value)
                    .unwrap_or(0);
                let (id, name) = s
                    .attributes
                    .as_ref()
                    .map(|a| {
                        (
                            a.id.clone(),
                            a.metadata
                                .as_ref()
                                .map(|m| m.name.clone())
                                .unwrap_or_default(),
                        )
                    })
                    .unwrap_or_default();
                render(&id, &name, mem, cpu);
            }
        }
    }
    Ok(())
}

async fn cmd_info(r: &mut RClient, i: &mut IClient) -> Result<(), String> {
    let st = r
        .status(Request::new(StatusRequest::default()))
        .await
        .map_err(status)?
        .into_inner();
    if let Some(rs) = st.status {
        for c in rs.conditions {
            println!("{}: {}", c.r#type, if c.status { "Ready" } else { "NotReady" });
        }
        let handlers: Vec<String> = st.runtime_handlers.iter().map(|h| h.name.clone()).collect();
        println!("Handlers: {}", handlers.join(", "));
    }
    let fs = i
        .image_fs_info(Request::new(ImageFsInfoRequest::default()))
        .await
        .map_err(status)?
        .into_inner();
    if let Some(f) = fs.image_filesystems.first() {
        let mp = f
            .fs_id
            .as_ref()
            .map(|x| x.mountpoint.clone())
            .unwrap_or_default();
        println!("ImageFS mountpoint: {}", mp);
        println!(
            "ImageFS used: {}",
            human_size(f.used_bytes.as_ref().map(|v| v.value).unwrap_or(0))
        );
    }
    Ok(())
}

async fn cmd_version(r: &mut RClient) -> Result<(), String> {
    let v = r
        .version(Request::new(VersionRequest::default()))
        .await
        .map_err(status)?
        .into_inner();
    println!("Client version:   {}", nautilus::VERSION);
    println!("Runtime name:     {}", v.runtime_name);
    println!("Runtime version:  {}", v.runtime_version);
    println!("API version:      {}", v.runtime_api_version);
    Ok(())
}
