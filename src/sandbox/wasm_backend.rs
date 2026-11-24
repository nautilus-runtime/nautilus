//! wasm-backed sandbox: WASM/WASI workloads executed in-process by wasmtime.
//!
//! No rootfs, no process tree: the pod workload is a `.wasm` (or `.wat`) module
//! executed on an embedded engine inside the nautilus process itself — this is
//! what makes the Rust implementation genuinely different from shelling out.
//! WASI stdout and stderr share the workload log file.

use crate::core::{Config, Error, Result};
use crate::sandbox::{SandboxBackend, SandboxKind, StartOutcome, WorkloadSpec};
use async_trait::async_trait;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use wasmtime::{Engine, Linker, Module, Store};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::{
    HostOutputStream, StdoutStream, StreamError, StreamResult, Subscribe, WasiCtxBuilder,
};

pub struct WasmBackend {
    config: Arc<Config>,
    engine: Engine,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ImageCommand {
    module_name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

fn local_module(reference: &str) -> Option<&Path> {
    if let Some(path) = reference.strip_prefix("file://") {
        return Some(Path::new(path));
    }
    let path = Path::new(reference);
    let legacy_path = path
        .extension()
        .is_some_and(|ext| ext == "wasm" || ext == "wat")
        && path.is_file();
    (path.is_absolute() || legacy_path).then_some(path)
}

async fn image_module(root: &Path, entrypoint: &str) -> Result<PathBuf> {
    let path = Path::new(entrypoint);
    let relative = path.strip_prefix("/").map_err(|_| {
        Error::Image("WASI image module must use an absolute path inside the image".into())
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(Error::Image("invalid WASI image module path".into()));
    }
    let resolved = tokio::fs::canonicalize(root.join(relative)).await?;
    if !resolved.starts_with(root) || !tokio::fs::metadata(&resolved).await?.is_file() {
        return Err(Error::Image(
            "WASI module must be a regular file inside the image".into(),
        ));
    }
    Ok(resolved)
}

#[derive(Clone)]
struct LogStream {
    path: PathBuf,
    channel: &'static str,
}

impl StdoutStream for LogStream {
    fn stream(&self) -> Box<dyn HostOutputStream> {
        Box::new(self.clone())
    }

    fn isatty(&self) -> bool {
        false
    }
}

#[async_trait]
impl Subscribe for LogStream {
    async fn ready(&mut self) {}
}

impl HostOutputStream for LogStream {
    fn write(&mut self, bytes: bytes::Bytes) -> StreamResult<()> {
        use std::io::Write;
        // Reopen on output so kubelet can rotate the log file while a job runs.
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| StreamError::LastOperationFailed(e.into()))?;
        let time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        for piece in bytes.split_inclusive(|&byte| byte == b'\n') {
            let tag = if piece.ends_with(b"\n") { 'F' } else { 'P' };
            let mut record = format!("{time} {} {tag} ", self.channel).into_bytes();
            record.extend_from_slice(piece);
            if tag == 'P' {
                record.push(b'\n');
            }
            file.write_all(&record)
                .map_err(|e| StreamError::LastOperationFailed(e.into()))?;
        }
        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(16 * 1024)
    }
}

impl WasmBackend {
    pub fn new(config: Arc<Config>) -> Result<Self> {
        Ok(Self {
            config,
            engine: Engine::default(),
        })
    }

    fn prepared(&self, id: &str) -> PathBuf {
        self.config.storage.container_dir.join(format!("{id}-wasi"))
    }

    /// Compile + instantiate + run one WASI command to completion.
    fn run_module_blocking(
        engine: Engine,
        path: PathBuf,
        command: ImageCommand,
        log: PathBuf,
    ) -> Result<i32> {
        let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
        preview1::add_to_linker_sync(&mut linker, |t| t)
            .map_err(|e| Error::Runtime(format!("wasi linker setup failed: {e}")))?;

        let mut builder = WasiCtxBuilder::new();
        builder.stdout(LogStream {
            path: log.clone(),
            channel: "stdout",
        });
        builder.stderr(LogStream {
            path: log,
            channel: "stderr",
        });
        let mut argv = vec![command.module_name];
        argv.extend(command.args);
        builder.args(&argv);
        for (k, v) in command.env {
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
        self.prepare_image(spec).await.map(|_| ())
    }

    async fn prepare_image(&self, spec: &WorkloadSpec) -> Result<String> {
        if let Some(path) = local_module(&spec.image_ref) {
            Module::from_file(&self.engine, path)
                .map_err(|e| Error::Image(format!("compile local WASI module: {e}")))?;
            return Ok(String::new());
        }
        if spec.image_ref.starts_with("file://") {
            return Err(Error::Image(
                "file:// WASI module paths must be absolute".into(),
            ));
        }
        let storage = &self.config.storage.container_dir;
        tokio::fs::create_dir_all(storage).await?;
        let stage = tempfile::tempdir_in(storage)?;
        let rootfs = stage.path().join("rootfs");
        tokio::fs::create_dir(&rootfs).await?;
        let image =
            crate::storage::pull::materialize_rootfs(&self.config, &spec.image_ref, &rootfs)
                .await?;
        let (entrypoint, args) = if spec.entrypoint.is_empty() {
            (
                &image.entrypoint,
                if spec.args.is_empty() {
                    &image.cmd
                } else {
                    &spec.args
                },
            )
        } else {
            (&spec.entrypoint, &spec.args)
        };
        let (module_name, entry_args) = entrypoint.split_first().ok_or_else(|| {
            Error::Image("WASI image requires a module path in its OCI entrypoint".into())
        })?;
        let resolved = image_module(&rootfs.canonicalize()?, module_name).await?;
        Module::from_file(&self.engine, &resolved)
            .map_err(|e| Error::Image(format!("compile OCI WASI module: {e}")))?;
        tokio::fs::copy(&resolved, stage.path().join("module.wasm")).await?;
        tokio::fs::remove_dir_all(&rootfs).await?;
        let mut env: std::collections::BTreeMap<_, _> = image
            .env
            .iter()
            .filter_map(|entry| entry.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect();
        env.extend(spec.env.clone());
        let command = ImageCommand {
            module_name: module_name.clone(),
            args: entry_args.iter().chain(args).cloned().collect(),
            env: env.into_iter().collect(),
        };
        tokio::fs::write(
            stage.path().join("command.json"),
            serde_json::to_vec(&command)?,
        )
        .await?;
        tokio::fs::rename(stage.path(), self.prepared(&spec.id)).await?;
        Ok(image.image_id)
    }

    async fn start(&self, spec: &WorkloadSpec) -> Result<StartOutcome> {
        let engine = self.engine.clone();
        let (path, command) = if let Some(path) = local_module(&spec.image_ref) {
            (
                path.to_owned(),
                ImageCommand {
                    module_name: path.display().to_string(),
                    args: spec.args.clone(),
                    env: spec.env.clone().into_iter().collect(),
                },
            )
        } else {
            let prepared = self.prepared(&spec.id);
            let data = tokio::fs::read(prepared.join("command.json")).await?;
            (prepared.join("module.wasm"), serde_json::from_slice(&data)?)
        };

        tokio::fs::create_dir_all(&spec.output_dir).await?;
        let log = spec.output_dir.join(&spec.log_relpath);
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .await?;

        let exit_code = tokio::task::spawn_blocking(move || {
            Self::run_module_blocking(engine, path, command, log)
        })
        .await
        .map_err(|e| Error::Runtime(format!("wasm worker join failed: {e}")))??;

        tracing::info!(container_id = %spec.id, exit_code, "wasm workload finished");
        Ok(StartOutcome::Finished { exit_code })
    }

    async fn stop(&self, _id: &str, _grace: std::time::Duration) -> Result<Option<i32>> {
        // Batch workloads run to completion inside start(); there is nothing to stop.
        Ok(None)
    }

    async fn remove(&self, id: &str) -> Result<()> {
        match tokio::fs::remove_dir_all(self.prepared(id)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
