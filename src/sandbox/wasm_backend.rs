//! wasm-backed sandbox: WASM/WASI workloads executed in-process by wasmtime.
//!
//! No rootfs, no process tree: the pod workload is a `.wasm` (or `.wat`) module
//! executed on an embedded engine inside the nautilus process itself — this is
//! what makes the Rust implementation genuinely different from shelling out.
//! WASI stdout and stderr share the workload log file.

use crate::core::{Config, Error, Result};
use crate::sandbox::{SandboxBackend, SandboxKind, StartOutcome, WorkloadSpec};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, watch};
use wasmtime::{Engine, ExternType, InstancePre, Linker, Module, Store};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::{pipe::AsyncWriteStream, AsyncStdoutStream, WasiCtxBuilder};

pub struct WasmBackend {
    #[allow(dead_code)]
    config: Arc<Config>,
    engine: Arc<Engine>,
    jobs: Mutex<HashMap<String, Job>>,
}

#[derive(Clone)]
struct Job {
    cancel: watch::Sender<bool>,
    finished: watch::Receiver<Option<i32>>,
}

fn module_path(image_ref: &str) -> Result<PathBuf> {
    let raw = image_ref.strip_prefix("file://").unwrap_or(image_ref);
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Err(Error::Image(
            "remote wasm modules are not supported yet; use file:// paths".into(),
        ));
    }
    Ok(PathBuf::from(raw))
}

impl WasmBackend {
    pub fn new(config: Arc<Config>) -> Result<Self> {
        let mut engine_config = wasmtime::Config::new();
        engine_config.async_support(true).epoch_interruption(true);
        let engine = Arc::new(
            Engine::new(&engine_config)
                .map_err(|e| Error::Runtime(format!("initialize WASI engine: {e}")))?,
        );
        let ticker = Arc::downgrade(&engine);
        // This must advance even when every async worker is running guest code.
        std::thread::Builder::new()
            .name("wasmtime-epoch".into())
            .spawn(move || {
                while let Some(engine) = ticker.upgrade() {
                    engine.increment_epoch();
                    drop(engine);
                    std::thread::sleep(Duration::from_millis(10));
                }
            })?;
        Ok(Self {
            config,
            engine,
            jobs: Mutex::new(HashMap::new()),
        })
    }

    async fn compile(&self, path: PathBuf) -> Result<InstancePre<WasiP1Ctx>> {
        let engine = self.engine.clone();
        tokio::task::spawn_blocking(move || {
            let module = Module::from_file(&engine, path)
                .map_err(|e| Error::Runtime(format!("compile wasm module: {e}")))?;
            if !matches!(module.get_export("_start"), Some(ExternType::Func(start))
                if start.params().len() == 0 && start.results().len() == 0)
            {
                return Err(Error::Runtime(
                    "WASI module must export _start with no parameters or results".into(),
                ));
            }
            let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
            preview1::add_to_linker_async(&mut linker, |t| t)
                .map_err(|e| Error::Runtime(format!("WASI linker setup failed: {e}")))?;
            linker
                .instantiate_pre(&module)
                .map_err(|e| Error::Runtime(format!("link WASI module: {e}")))
        })
        .await
        .map_err(|e| Error::Runtime(format!("WASI compiler task failed: {e}")))?
    }

    async fn run_module(
        engine: Arc<Engine>,
        module: InstancePre<WasiP1Ctx>,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        log: tokio::fs::File,
    ) -> Result<i32> {
        let mut builder = WasiCtxBuilder::new();
        // Preview 1 flushes each write before returning to the guest. Use the
        // bounded async stream so filesystem writes do not block a CRI worker.
        builder.stdout(AsyncStdoutStream::new(AsyncWriteStream::new(
            16 * 1024,
            log.try_clone().await?,
        )));
        builder.stderr(AsyncStdoutStream::new(AsyncWriteStream::new(
            16 * 1024,
            log,
        )));
        builder.args(&argv);
        for (k, v) in env {
            builder.env(&k, &v);
        }
        let wasi_ctx = builder.build_p1();

        let mut store = Store::new(&engine, wasi_ctx);
        store.set_epoch_deadline(1);
        store.epoch_deadline_async_yield_and_update(1);
        let instance = module
            .instantiate_async(&mut store)
            .await
            .map_err(|e| Error::Runtime(format!("instantiate wasm module: {e}")))?;
        let start = instance
            .get_typed_func::<(), ()>(&mut store, "_start")
            .map_err(|e| Error::Runtime(format!("module has no _start export: {e}")))?;

        match start.call_async(&mut store, ()).await {
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
                Err(Error::Runtime(format!("wasm workload trapped: {err:#}")))
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
        self.compile(path).await?;
        Ok(())
    }

    async fn start(&self, spec: &WorkloadSpec) -> Result<StartOutcome> {
        let path = module_path(&spec.image_ref)?;
        let mut argv = vec![path.display().to_string()];
        argv.extend(spec.args.iter().cloned());
        let env = spec.env.clone().into_iter().collect();
        let module = self.compile(path).await?;

        tokio::fs::create_dir_all(&spec.output_dir).await?;
        let log = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(spec.output_dir.join(&spec.log_relpath))
            .await?;
        let (activate, active) = oneshot::channel();
        let (cancel, mut cancelled) = watch::channel(false);
        let (completed, finished) = watch::channel(None);
        {
            let mut jobs = self.jobs.lock().unwrap();
            if jobs
                .get(&spec.id)
                .is_some_and(|job| job.finished.borrow().is_none())
            {
                return Err(Error::Container(format!(
                    "WASI workload {} is already active",
                    spec.id
                )));
            }
            jobs.insert(
                spec.id.clone(),
                Job {
                    cancel,
                    finished: finished.clone(),
                },
            );
        }
        let engine = self.engine.clone();
        let id = spec.id.clone();
        tokio::spawn(async move {
            let run = async {
                if active.await.is_err() {
                    return 137;
                }
                match Self::run_module(engine, module, argv, env, log).await {
                    Ok(exit) => exit,
                    Err(error) => {
                        tracing::warn!(container_id = %id, %error, "WASI workload failed");
                        1
                    }
                }
            };
            let exit = tokio::select! {
                biased;
                _ = cancelled.wait_for(|cancel| *cancel) => 137,
                exit = run => exit,
            };
            completed.send_replace(Some(exit));
        });
        Ok(StartOutcome::Managed { activate, finished })
    }

    async fn stop(&self, id: &str, grace: Duration) -> Result<Option<i32>> {
        let Some(Job {
            cancel,
            mut finished,
        }) = self.jobs.lock().unwrap().get(id).cloned()
        else {
            return Ok(None);
        };
        if let Ok(outcome) = tokio::time::timeout(grace, finished.wait_for(Option::is_some)).await {
            return Ok(Some(outcome.ok().and_then(|exit| *exit).unwrap_or(1)));
        }
        cancel.send_replace(true);
        let outcome = finished.wait_for(Option::is_some).await;
        Ok(Some(outcome.ok().and_then(|exit| *exit).unwrap_or(1)))
    }

    async fn remove(&self, id: &str) -> Result<()> {
        self.stop(id, Duration::ZERO).await?;
        self.jobs.lock().unwrap().remove(id);
        Ok(())
    }
}
