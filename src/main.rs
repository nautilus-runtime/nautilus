use clap::{Arg, Command};
use nautilus::core::{Config, StateManager};
use nautilus::network::NetworkManager;
use nautilus::proto::runtime::v1::image_service_server::ImageServiceServer;
use nautilus::proto::runtime::v1::runtime_service_server::RuntimeServiceServer;
use nautilus::{NautilusImageService, NautilusRuntimeService};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::signal;
use tonic::transport::Server;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const APP_NAME: &str = "nautilusd";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse command line arguments
    let matches = Command::new(APP_NAME)
        .version(nautilus::VERSION)
        .about("High-performance CRI-compatible container runtime")
        .arg(
            Arg::new("config")
                .short('c')
                .long("config")
                .value_name("FILE")
                .help("Configuration file (default: /etc/nautilus/config.toml)"),
        )
        .arg(
            Arg::new("socket")
                .short('s')
                .long("socket")
                .value_name("PATH")
                .help("Override the Unix socket from the configuration"),
        )
        .arg(
            Arg::new("log-level")
                .short('l')
                .long("log-level")
                .value_name("LEVEL")
                .help("Log level (trace, debug, info, warn, error)")
                .default_value("info"),
        )
        .arg(
            Arg::new("dump-config")
                .long("dump-config")
                .value_name("FILE")
                .help("Write the effective default configuration to FILE and exit"),
        )
        .get_matches();

    // Ops convenience: materialize a full config skeleton for editing.
    if let Some(path) = matches.get_one::<String>("dump-config") {
        let cfg = nautilus::Config::default();
        nautilus::Config::to_file(&cfg, path)?;
        eprintln!("Default configuration written to {path}");
        return Ok(());
    }

    // Initialize tracing
    let log_level = matches.get_one::<String>("log-level").unwrap();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // Bare level keeps library-crate targets (`nautilus::*`) visible;
                // a `<bin>=<lvl>` filter would silence every lib event.
                .unwrap_or_else(|_| log_level.clone().into()),
        )
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();
    // Library crates using the `log` facade (spdystream-rs) become visible.
    tracing_log::LogTracer::init().ok();

    info!("Starting {} v{}", APP_NAME, nautilus::VERSION);

    let explicit_path = matches.get_one::<String>("config");
    let config_path = explicit_path.map(String::as_str).unwrap_or("/etc/nautilus/config.toml");
    let config = match Config::from_file(config_path) {
        Ok(config) => {
            info!("Loaded configuration from {}", config_path);
            config
        }
        Err(nautilus::Error::Io(e))
            if explicit_path.is_none() && e.kind() == std::io::ErrorKind::NotFound =>
        {
            Config::default()
        }
        Err(e) => return Err(format!("Failed to load config from {config_path}: {e}").into()),
    };

    // Validate configuration
    if let Err(e) = config.validate() {
        error!("Configuration validation failed: {}", e);
        std::process::exit(1);
    }

    // Override socket path from command line
    let socket_path = matches.get_one::<String>("socket").cloned().unwrap_or_else(|| {
        config.cri.listen_address.trim_start_matches("unix://").to_owned()
    });
    
    // Initialize state manager
    let state_dir = &config.runtime.state_dir;
    fs::create_dir_all(state_dir)?;
    let db_path = state_dir.join("state.db");
    let state_manager = Arc::new(StateManager::new(db_path)?);
    
    info!("State manager initialized at {:?}", state_dir);

    // Finish reconciliation before serving requests that can restart a container.
    if let Err(e) = nautilus::network::publish::sweep_network(&state_manager).await {
        warn!(error = %e, "Skipped startup cleanup of published ports");
    }

    // Initialize services
    let config_arc = Arc::new(config);

    // Runtime managers (pod/container/sandbox) are wired inside
    // NautilusRuntimeService::new, keyed by sandbox registry routing.
    let runtime_service = NautilusRuntimeService::new(
        config_arc.clone(),
        state_manager.clone(),
    );
    let image_service = NautilusImageService::new(config_arc.clone(), state_manager.clone());
    
    // Initialize network manager
    let network_manager = NetworkManager::new(config_arc.clone());
    network_manager.initialize().await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("Failed to initialize network manager: {}", e)))?;

    // Initialize image service
    image_service.initialize().await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("Failed to initialize image service: {}", e)))?;

    // Setup socket
    let path = Path::new(&socket_path);
    if path.exists() {
        info!("Removing existing socket file: {}", socket_path);
        fs::remove_file(&socket_path)?;
    } else if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    info!("Binding to socket: {}", socket_path);
    let uds = UnixListener::bind(&socket_path)?;
    // The CRI socket is a root-equivalent interface (it can start privileged
    // containers). Match containerd/CRI-O convention: 0660 root:root —
    // kubelet runs as root; unrelated local users must not reach it.
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o660))?;
    
    // Start gRPC server
    let server = Server::builder()
        .add_service(RuntimeServiceServer::new(runtime_service))
        .add_service(ImageServiceServer::new(image_service))
        .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds));

    info!("Nautilus CRI server listening on {}", socket_path);

    // Handle shutdown gracefully: SIGINT (interactive Ctrl+C) and SIGTERM
    // (systemd stop / k8s node drain) both take the cleanup path below.
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = server => {
            if let Err(e) = result {
                error!("Server error: {}", e);
            }
        }
        _ = signal::ctrl_c() => {
            info!("Received SIGINT, shutting down gracefully...");
        }
        _ = sigterm.recv() => {
            info!("Received SIGTERM, shutting down gracefully...");
        }
    }

    // Cleanup
    if path.exists() {
        info!("Cleaning up socket file");
        fs::remove_file(&socket_path)?;
    }

    info!("Nautilus shutdown complete");
    Ok(())
}
