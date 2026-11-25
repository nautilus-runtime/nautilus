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
use std::future::Future;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{oneshot, watch};
use wasmtime::{Engine, ExternType, InstancePre, Linker, Module, Store};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::{pipe::AsyncWriteStream, AsyncStdoutStream, WasiCtxBuilder};

pub struct WasmBackend {
    config: Arc<Config>,
    engine: Arc<Engine>,
    jobs: Mutex<HashMap<String, Job>>,
}

#[derive(Clone)]
struct Job {
    cancel: watch::Sender<bool>,
    finished: watch::Receiver<Option<i32>>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ImageCommand {
    module_name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

fn module_path(reference: &str) -> &Path {
    Path::new(reference.strip_prefix("file://").unwrap_or(reference))
}

fn copy_image_module(root: &Path, path: &Path, destination: &Path) -> Result<()> {
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
    let root = pathrs::Root::open(root)?;
    let mut input = root.open_subpath(
        relative,
        pathrs::flags::OpenFlags::O_RDONLY | pathrs::flags::OpenFlags::O_NONBLOCK,
    )?;
    if !input.metadata()?.is_file() {
        return Err(Error::Image(
            "WASI module must be a regular file inside the image".into(),
        ));
    }
    io::copy(&mut input, &mut std::fs::File::create(destination)?)?;
    Ok(())
}

type WriteRecord = Pin<Box<dyn Future<Output = io::Result<usize>> + Send>>;

struct CriLogWriter {
    path: PathBuf,
    channel: &'static str,
    pending: Option<WriteRecord>,
}

impl CriLogWriter {
    fn stream(path: PathBuf, channel: &'static str) -> AsyncStdoutStream {
        AsyncStdoutStream::new(AsyncWriteStream::new(
            16 * 1024,
            Self {
                path,
                channel,
                pending: None,
            },
        ))
    }
}

impl AsyncWrite for CriLogWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let path = self.path.clone();
        let channel = self.channel;
        let write = self.pending.get_or_insert_with(|| {
            let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            let mut records = Vec::new();
            for piece in bytes.split_inclusive(|&byte| byte == b'\n') {
                let tag = if piece.ends_with(b"\n") { 'F' } else { 'P' };
                records.extend_from_slice(format!("{timestamp} {channel} {tag} ").as_bytes());
                records.extend_from_slice(piece);
                if tag == 'P' {
                    records.push(b'\n');
                }
            }
            let len = bytes.len();
            Box::pin(async move {
                // Open the current path so a kubelet rotation can take effect.
                let mut file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .await?;
                file.write_all(&records).await?;
                file.flush().await?;
                Ok(len)
            })
        });
        match write.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None;
                Poll::Ready(result)
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

pub(crate) async fn recover_staging(
    config: &Config,
    state: &crate::core::StateManager,
) -> Result<()> {
    let mut dirs = match tokio::fs::read_dir(config.runtime.state_dir.join("wasi")).await {
        Ok(dirs) => dirs,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    while let Some(dir) = dirs.next_entry().await? {
        if !dir.file_type().await?.is_dir() {
            continue;
        }
        let name = dir.file_name();
        let Some(name) = name.to_str() else { continue };
        let abandoned = name.starts_with(".preparing-")
            || (uuid::Uuid::parse_str(name).is_ok_and(|id| id.to_string() == name)
                && state.get_container_state(name).await?.is_none());
        if abandoned {
            tokio::fs::remove_dir_all(dir.path()).await?;
        }
    }
    Ok(())
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

    fn prepared(&self, id: &str) -> PathBuf {
        self.config.runtime.state_dir.join("wasi").join(id)
    }

    async fn compile(&self, path: PathBuf) -> Result<Module> {
        let engine = self.engine.clone();
        tokio::task::spawn_blocking(move || Module::from_file(&engine, path))
            .await
            .map_err(|e| Error::Runtime(format!("WASI compiler task failed: {e}")))?
            .map_err(|e| Error::Runtime(format!("compile wasm module: {e}")))
    }

    fn link(&self, module: &Module) -> Result<InstancePre<WasiP1Ctx>> {
        if !matches!(module.get_export("_start"), Some(ExternType::Func(start))
            if start.params().len() == 0 && start.results().len() == 0)
        {
            return Err(Error::Runtime(
                "WASI module must export _start with no parameters or results".into(),
            ));
        }
        let mut linker: Linker<WasiP1Ctx> = Linker::new(&self.engine);
        preview1::add_to_linker_async(&mut linker, |ctx| ctx)
            .map_err(|e| Error::Runtime(format!("WASI linker setup failed: {e}")))?;
        linker
            .instantiate_pre(module)
            .map_err(|e| Error::Runtime(format!("link WASI module: {e}")))
    }

    async fn run_module(
        engine: Arc<Engine>,
        module: InstancePre<WasiP1Ctx>,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        log: PathBuf,
        packaged: bool,
    ) -> Result<i32> {
        let mut builder = WasiCtxBuilder::new();
        if packaged {
            builder.stdout(CriLogWriter::stream(log.clone(), "stdout"));
            builder.stderr(CriLogWriter::stream(log, "stderr"));
        } else {
            let file = tokio::fs::OpenOptions::new().append(true).open(log).await?;
            builder.stdout(AsyncStdoutStream::new(AsyncWriteStream::new(16 * 1024, file.try_clone().await?)));
            builder.stderr(AsyncStdoutStream::new(AsyncWriteStream::new(16 * 1024, file)));
        }
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
        self.prepare_image(spec).await.map(|_| ())
    }

    async fn prepare_image(&self, spec: &WorkloadSpec) -> Result<String> {
        let path = module_path(&spec.image_ref);
        if spec.image_ref.starts_with("file://") || path.is_absolute() || path.is_file() {
            self.compile(path.to_owned()).await?;
            return Ok(String::new());
        }
        let storage = self.config.runtime.state_dir.join("wasi");
        tokio::fs::create_dir_all(&storage).await?;
        let stage = tempfile::Builder::new()
            .prefix(".preparing-")
            .tempdir_in(storage)?;
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
        let (module_name, entry_args, args) = if let Some((module, rest)) = entrypoint.split_first() {
            (module, rest, args.as_slice())
        } else {
            let (module, rest) = args.split_first().ok_or_else(|| {
                Error::Image("WASI image requires an executable module path".into())
            })?;
            (module, rest, &[][..])
        };
        let packaged = stage.path().join("module.wasm");
        let source = PathBuf::from(module_name);
        let staged = packaged.clone();
        let root = rootfs.clone();
        tokio::task::spawn_blocking(move || copy_image_module(&root, &source, &staged))
            .await
            .map_err(|e| Error::Runtime(format!("WASI module staging task failed: {e}")))??;
        self.compile(packaged).await?;
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
        let packaged = !spec.image_id.is_empty();
        let (path, command) = if !packaged {
            let path = module_path(&spec.image_ref);
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
        let mut argv = vec![command.module_name];
        argv.extend(command.args);
        let env = command.env;
        let module = self.link(&self.compile(path).await?)?;

        tokio::fs::create_dir_all(&spec.output_dir).await?;
        let log = spec.output_dir.join(&spec.log_relpath);
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
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
                match Self::run_module(engine, module, argv, env, log, packaged).await {
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
        match tokio::fs::remove_dir_all(self.prepared(id)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
