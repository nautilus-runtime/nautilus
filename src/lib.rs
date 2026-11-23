pub mod core;
pub mod cri;
pub mod policy;
pub mod runtime;
pub mod sandbox;
pub mod storage;
pub mod network;
pub mod proto;

// Re-export commonly used types
pub use core::{Config, Error, Result, StateManager};
pub use cri::{NautilusImageService, NautilusRuntimeService};
pub use runtime::{ContainerManager, PodManager};
pub use sandbox::{SandboxBackend, SandboxKind, SandboxRegistry};

// Version information
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const RUNTIME_NAME: &str = "nautilus";
pub const RUNTIME_API_VERSION: &str = "v1";

