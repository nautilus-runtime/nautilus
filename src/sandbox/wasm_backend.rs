//! wasm-backed sandbox: WASM/WASI workloads executed in-process by wasmtime.
//!
//! No rootfs, no process tree: the pod workload is a `.wasm` (or `.wat`) module
//! executed on an embedded engine inside the nautilus process itself — this is
//! what makes the Rust implementation genuinely different from shelling out.
//! WASI stdout and stderr share the workload log file.

use crate::core::{Config, Error, Result};
use crate::sandbox::init_io::{InitIo, ATTACH_CHUNK_BYTES};
use crate::sandbox::{SandboxBackend, SandboxKind, StartOutcome, WorkloadSpec};
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use wasmtime::{Engine, ExternType, InstancePre, Linker, Module, Store};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::{
    pipe::AsyncWriteStream, AsyncStdoutStream, HostInputStream, StdinStream, StreamError,
    Subscribe, WasiCtxBuilder,
};

pub struct WasmBackend {
    config: Arc<Config>,
    engine: Arc<Engine>,
    jobs: Mutex<HashMap<String, Job>>,
}

#[derive(Clone)]
struct Job {
    cancel: watch::Sender<bool>,
    finished: watch::Receiver<Option<i32>>,
    io: Arc<InitIo>,
}

struct WasiStdio {
    io: Arc<InitIo>,
    stdin: Option<mpsc::Receiver<Vec<u8>>>,
    log: PathBuf,
    packaged: bool,
}

#[derive(Clone)]
struct WasiStdin(Arc<tokio::sync::Mutex<WasiInput>>);

struct WasiInput {
    reader: mpsc::Receiver<Vec<u8>>,
    pending: Bytes,
}

impl StdinStream for WasiStdin {
    fn stream(&self) -> Box<dyn HostInputStream> {
        Box::new(self.clone())
    }

    fn isatty(&self) -> bool {
        false
    }
}

#[async_trait]
impl HostInputStream for WasiStdin {
    fn read(&mut self, size: usize) -> std::result::Result<Bytes, StreamError> {
        let mut input = self
            .0
            .try_lock()
            .map_err(|_| StreamError::trap("concurrent WASI stdin reads"))?;
        if input.pending.is_empty() {
            match input.reader.try_recv() {
                Ok(bytes) => input.pending = bytes.into(),
                Err(mpsc::error::TryRecvError::Empty) => return Ok(Bytes::new()),
                // Receiver disconnect stays observable on every subsequent read.
                Err(mpsc::error::TryRecvError::Disconnected) => return Err(StreamError::Closed),
            }
        }
        let size = size.min(input.pending.len());
        Ok(input.pending.split_to(size))
    }
}

#[async_trait]
impl Subscribe for WasiStdin {
    async fn ready(&mut self) {
        let mut input = self.0.lock().await;
        if input.pending.is_empty() {
            input.pending = input.reader.recv().await.unwrap_or_default().into();
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::workload::StreamOptions;
    use std::path::Path;

    #[test]
    fn resolves_absolute_module_symlinks_inside_the_image() {
        let stage = tempfile::tempdir().unwrap();
        let root = stage.path().join("rootfs");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("command.wat"), b"image module").unwrap();
        std::os::unix::fs::symlink("/command.wat", root.join("entry")).unwrap();
        std::os::unix::fs::symlink("../outside", root.join("escape")).unwrap();
        std::fs::write(stage.path().join("outside"), b"host data").unwrap();

        let module = stage.path().join("module");
        copy_image_module(&root, Path::new("/entry"), &module).unwrap();
        assert_eq!(std::fs::read(&module).unwrap(), b"image module");
        assert!(copy_image_module(&root, Path::new("/escape"), &module).is_err());
    }

    #[tokio::test]
    async fn preview1_attach_forwards_pipes_eof_and_cancellation() {
        tokio::time::timeout(Duration::from_secs(20), async {
            let temp = tempfile::tempdir().unwrap();
            let module = temp.path().join("pipe.wat");
            std::fs::write(
                &module,
                include_str!("../../tests/fixtures/wasi/pipe_stdio.wat"),
            )
            .unwrap();
            let mut config = Config::default();
            config.runtime.state_dir = temp.path().join("state");
            let backend = WasmBackend::new(Arc::new(config)).unwrap();
            let mut spec = WorkloadSpec {
                id: "pipe".into(),
                image_ref: module.to_string_lossy().into_owned(),
                image_id: String::new(),
                cgroup_parent: String::new(),
                args: Vec::new(),
                working_dir: String::new(),
                strict_groups: false,
                tty: false,
                stdin_once: true,
                entrypoint: Vec::new(),
                env: HashMap::new(),
                output_dir: temp.path().to_owned(),
                resources: None,
                security: None,
                network_ns: None,
                ipc_ns: None,
                log_relpath: "out.log".into(),
                hostname: None,
                dns_servers: Vec::new(),
                dns_searches: Vec::new(),
                dns_options: Vec::new(),
                volumes: Vec::new(),
                pid_ns: None,
                open_stdin: true,
            };
            let options = StreamOptions {
                stdin: true,
                stdout: true,
                stderr: true,
                tty: false,
            };
            for (packaged, expect_exit) in [(false, 37), (true, 37), (true, 137)] {
                if packaged && spec.image_id.is_empty() {
                    spec.image_id = "sha256:test".into();
                    let prepared = backend.prepared(&spec.id);
                    std::fs::create_dir_all(&prepared).unwrap();
                    std::fs::copy(&module, prepared.join("module.wasm")).unwrap();
                    std::fs::write(
                        prepared.join("command.json"),
                        br#"{"module_name":"pipe","args":[],"env":[]}"#,
                    )
                    .unwrap();
                    spec.log_relpath = "cri.log".into();
                }
                let StartOutcome::Managed {
                    activate,
                    mut finished,
                } = backend.start(&spec).await.unwrap()
                else {
                    panic!("WASI must launch a managed job");
                };
                activate.send(()).unwrap();
                let io = backend.init_io(&spec.id).unwrap();
                let (cancel, _closed) = watch::channel(false);
                let (mut streams, resize) = io.attach(options, false, cancel).unwrap();
                assert!(resize.is_none());
                assert_eq!(streams.stderr.recv().await.unwrap(), b"ready\n");
                if expect_exit == 37 {
                    let input = streams.stdin.take().unwrap();
                    let payload: Vec<_> = if packaged {
                        b"packaged\n".to_vec()
                    } else {
                        (0..16_384).map(|i| (i % 251) as u8).collect()
                    };
                    input.send(payload.clone()).await.unwrap();
                    drop(input);
                    let mut output = Vec::new();
                    while let Some(chunk) = streams.stdout.recv().await {
                        output.extend(chunk);
                    }
                    assert_eq!(output, payload);
                    let log = temp.path().join(&spec.log_relpath);
                    if packaged {
                        let log = std::fs::read_to_string(log).unwrap();
                        assert!(log.contains(" stderr F ready\n"), "{log}");
                        assert!(log.contains(" stdout F packaged\n"), "{log}");
                    } else {
                        let mut expected = b"ready\n".to_vec();
                        expected.extend(payload);
                        assert_eq!(std::fs::read(log).unwrap(), expected);
                    }
                } else {
                    assert_eq!(
                        backend.stop(&spec.id, Duration::ZERO).await.unwrap(),
                        Some(137)
                    );
                }
                assert_eq!(streams.exit.await.unwrap().unwrap(), expect_exit);
                assert_eq!(
                    *finished.wait_for(Option::is_some).await.unwrap(),
                    Some(expect_exit)
                );
            }
        })
        .await
        .expect("WASI pipe or cancellation blocked");
    }
}

type WriteRecord = Pin<Box<dyn Future<Output = io::Result<usize>> + Send>>;

struct CriLogWriter {
    path: PathBuf,
    channel: &'static str,
    pending: Option<WriteRecord>,
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

struct WorkloadLog<W> {
    log: W,
    io: Arc<InitIo>,
    stderr: bool,
}

impl<W: AsyncWrite + Send + Unpin + 'static> WorkloadLog<W> {
    fn stream(log: W, io: Arc<InitIo>, stderr: bool) -> AsyncStdoutStream {
        AsyncStdoutStream::new(AsyncWriteStream::new(
            ATTACH_CHUNK_BYTES,
            Self { log, io, stderr },
        ))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for WorkloadLog<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let count = std::task::ready!(Pin::new(&mut self.log).poll_write(cx, bytes))?;
        let output = if self.stderr {
            &self.io.stderr
        } else {
            &self.io.stdout
        };
        output.send(&bytes[..count]);
        Poll::Ready(Ok(count))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.log).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.log).poll_shutdown(cx)
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

    pub(super) fn init_io(&self, id: &str) -> Option<Arc<InitIo>> {
        self.jobs.lock().unwrap().get(id).map(|job| job.io.clone())
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
        stdio: WasiStdio,
    ) -> Result<i32> {
        let mut builder = WasiCtxBuilder::new();
        let WasiStdio {
            io,
            stdin,
            log,
            packaged,
        } = stdio;
        if let Some(stdin) = stdin {
            builder.stdin(WasiStdin(Arc::new(tokio::sync::Mutex::new(WasiInput {
                reader: stdin,
                pending: Bytes::new(),
            }))));
        }
        if packaged {
            let log_writer = |path, channel| CriLogWriter {
                path,
                channel,
                pending: None,
            };
            builder.stdout(WorkloadLog::stream(
                log_writer(log.clone(), "stdout"),
                io.clone(),
                false,
            ));
            builder.stderr(WorkloadLog::stream(log_writer(log, "stderr"), io, true));
        } else {
            let file = tokio::fs::OpenOptions::new().append(true).open(log).await?;
            builder.stdout(WorkloadLog::stream(
                file.try_clone().await?,
                io.clone(),
                false,
            ));
            builder.stderr(WorkloadLog::stream(file, io, true));
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
        let (module_name, entry_args, args) = if let Some((module, rest)) = entrypoint.split_first()
        {
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
        let (io, stdin) = InitIo::for_wasi(spec.open_stdin, spec.stdin_once);
        let io = Arc::new(io);
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
                    io: io.clone(),
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
                let stdio = WasiStdio {
                    io: io.clone(),
                    stdin,
                    log,
                    packaged,
                };
                match Self::run_module(engine, module, argv, env, stdio).await {
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
            io.stdout.finish();
            io.stderr.finish();
            io.finish(Ok(exit));
            completed.send_replace(Some(exit));
        });
        Ok(StartOutcome::Managed { activate, finished })
    }

    async fn stop(&self, id: &str, grace: Duration) -> Result<Option<i32>> {
        let Some(Job {
            cancel,
            mut finished,
            ..
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
