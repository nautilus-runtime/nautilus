//! wasm-backed sandbox: WASM/WASI workloads executed in-process by wasmtime.
//!
//! No rootfs, no process tree: the pod workload is a `.wasm` (or `.wat`) module
//! executed on an embedded engine inside the nautilus process itself — this is
//! what makes the Rust implementation genuinely different from shelling out.
//! Output currently inherits the daemon's stdio; per-container capture follows
//! once logging is centralized.

use crate::core::{Config, Error, Result};
use crate::sandbox::{SandboxBackend, SandboxKind, StartOutcome, WorkloadSpec};
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use wasmtime::{Engine, Linker, Module, Store};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::WasiCtxBuilder;

pub struct WasmBackend {
    #[allow(dead_code)]
    config: Arc<Config>,
    engine: Engine,
}

fn module_path(image_ref: &str) -> Result<PathBuf> {
    let raw = image_ref
        .strip_prefix("file://")
        .unwrap_or(image_ref);
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Err(Error::Image(
            "remote wasm modules are not supported yet; use file:// paths".into(),
        ));
    }
    Ok(PathBuf::from(raw))
}

impl WasmBackend {
    pub fn new(config: Arc<Config>) -> Result<Self> {
        Ok(Self {
            config,
            engine: Engine::default(),
        })
    }

    /// Compile + instantiate + run one WASI command to completion.
    fn run_module_blocking(
        engine: Engine,
        path: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Result<i32> {
        let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
        preview1::add_to_linker_sync(&mut linker, |t| t)
            .map_err(|e| Error::Runtime(format!("wasi linker setup failed: {e}")))?;

        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stdout();
        builder.inherit_stderr();
        // argv[0] convention: the module path itself.
        let mut argv = vec![path.display().to_string()];
        argv.extend(args);
        builder.args(&argv);
        for (k, v) in env {
            builder.env(&k, &v);
        }
        let wasi_ctx = builder.build_p1();

        let mut store = Store::new(&engine, wasi_ctx);
        let module = Module::from_file(&engine, &path)
            .map_err(|e| Error::Runtime(format!("compile wasm module: {e}")))?;
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| Error::Runtime(format!("instantiate wasm module: {e}")))?;
        let start = instance
            .get_typed_func::<(), ()>(&mut store, "_start")
            .map_err(|e| Error::Runtime(format!("module has no _start export: {e}")))?;

        match start.call(&mut store, ()) {
            Ok(()) => Ok(0),
            Err(err) => {
                // A WASI program calling proc_exit(n) surfaces as a trap carrying
                // I32Exit(n) somewhere in its chain — normal termination, not a
                // failure. anyhow's downcast walks the whole chain; fall back to
                // scanning the rendered chain for the marker text.
                if let Some(exit) = err.downcast_ref::<wasmtime_wasi::I32Exit>() {
                    return Ok(exit.0);
                }
                let rendered = format!("{err:#}");
                if let Some(pos) = rendered.find("Exited with i32 exit status ") {
                    let digits: String = rendered[pos + 27..]
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    if let Ok(code) = digits.parse::<i32>() {
                        return Ok(code);
                    }
                }
                Err(Error::Runtime(format!("wasm workload trapped: {err}")))
            }
        }
    }
}

#[async_trait]
impl SandboxBackend for WasmBackend {
    fn kind(&self) -> SandboxKind {
        SandboxKind::Wasm
    }

    async fn prepare(&self, spec: &WorkloadSpec) -> Result<()> {
        let path = module_path(&spec.image_ref)?;
        if !tokio::fs::try_exists(&path)
            .await
            .map_err(|e| Error::Image(format!("stat wasm module: {e}")))?
        {
            return Err(Error::Image(format!(
                "wasm module not found: {}",
                path.display()
            )));
        }
        // Fail fast: fully validate compilation at create-time, not start-time.
        Module::from_file(&self.engine, &path)
            .map_err(|e| Error::Runtime(format!("compile wasm module: {e}")))?;
        tracing::info!(container_id = %spec.id, module = %path.display(), "wasm module prepared");
        Ok(())
    }

    async fn start(&self, spec: &WorkloadSpec) -> Result<StartOutcome> {
        let path = module_path(&spec.image_ref)?;
        let engine = self.engine.clone();
        let args = spec.args.clone();
        let env: Vec<(String, String)> = spec.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

        let exit_code =
            tokio::task::spawn_blocking(move || Self::run_module_blocking(engine, path, args, env))
                .await
                .map_err(|e| Error::Runtime(format!("wasm worker join failed: {e}")))??;

        tracing::info!(container_id = %spec.id, exit_code, "wasm workload finished");
        Ok(StartOutcome::Finished { exit_code })
    }

    async fn stop(&self, _id: &str, _grace: std::time::Duration) -> Result<Option<i32>> {
        // Batch workloads run to completion inside start(); there is nothing to stop.
        Ok(None)
    }

    async fn remove(&self, _id: &str) -> Result<()> {
        Ok(())
    }
}
