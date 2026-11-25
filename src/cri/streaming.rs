//! CRI streaming server (exec / attach / port-forward) over SPDY/3.1.
//!
//! The CRI `Exec`/`Attach`/`PortForward` RPCs do not stream themselves; they
//! return a URL whose SPDY upgrade carries the actual session (remotecommand
//! v4.channel.k8s.io / portforward.k8s.io protocols). The client (kubelet,
//! critest) connects to that URL, sends one HTTP/1.1 `Upgrade: spdy/3.1`
//! request, and then multiplexes per-channel streams. Channels are
//! distinguished by the `streamType` header each SYN_STREAM carries — NOT by
//! stream ids:
//!
//! * exec/attach: `error` (exit status as a metav1.Status JSON), `stdin`,
//!   `stdout`, `stderr`, `resize` (tty sizes; ignored without a pty).
//! * port-forward: `error` plus one `data` stream per forwarded TCP
//!   connection, each carrying `port` (the container-side port) and
//!   `requestID` headers.
//!
//! The token is the only handle to the session: the CRI RPC registers every
//! request parameter (container, argv, tty, ports) here, and the client
//! passes nothing except the URL. Sessions are single-use (removed on
//! connect), like kubelet's streaming cache.
//!
//! spdystream-rs's `server::accept` parses the upgrade request but drops it,
//! so the connection setup below replicates it with `parse_upgrade_request` +
//! `send_upgrade_response_with_protocol` + `Connection::serve`, keeping the
//! parsed path to resolve the session. The session is captured per connection
//! in the stream handler closure. client-go can open the error channel on
//! stream 1. An untyped upgrade stream only needs an acknowledgment.

use crate::core::state::StateManager;
use crate::runtime::container::ContainerManager;
use crate::sandbox::runc_backend::InitIo;
use crate::vm::workload::{Operation, PortOptions, StreamOptions};
use bytes::Bytes;
use http::HeaderMap;
use spdystream_rs::connection::Connection;
use spdystream_rs::frame::RstStatus;
use spdystream_rs::server::{parse_upgrade_request, send_upgrade_response_with_protocol};
use spdystream_rs::stream::Stream;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch, OnceCell};

/// What the client asked for at CRI RPC time.
#[derive(Clone, Debug)]
pub enum SessionKind {
    Exec,
    /// Connect to the original workload's stdio.
    Attach,
    PortForward,
}

/// Everything needed to serve one streaming connection, registered by the CRI
/// RPC and looked up by token at connect time.
#[derive(Clone, Debug)]
pub struct Session {
    pub kind: SessionKind,
    pub container_id: String,
    /// Sandbox whose IP receives port-forward dials.
    pub pod_id: String,
    pub cmd: Vec<String>,
    pub ports: Vec<u16>,
    pub tty: bool,
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    native_attach: Option<Weak<InitIo>>,
}

static SESSIONS: Mutex<Option<HashMap<String, Session>>> = Mutex::new(None);
static STREAM_PORT: OnceCell<u16> = OnceCell::const_new();

fn sessions() -> std::sync::MutexGuard<'static, Option<HashMap<String, Session>>> {
    SESSIONS.lock().unwrap()
}

fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Register a streaming session and return its single-use token.
fn register(session: Session) -> String {
    let token = new_token();
    let mut g = sessions();
    g.get_or_insert_with(HashMap::new)
        .insert(token.clone(), session);
    token
}

/// Base URL the client connects to for a registered token. `127.0.0.1` is
/// correct for every deployment where the daemon and its client (kubelet or
/// critest) share a host — the only supported topology for now.
pub fn url_for(port: u16, mode: &str, token: &str) -> String {
    format!("http://127.0.0.1:{port}/{mode}/{token}")
}

/// Lazily bind the SPDY listener on the first streaming RPC and remember the
/// port for URL building. Every handler awaits this, so no startup-order
/// coupling with the gRPC server.
async fn ensure_started(
    exec: Arc<ContainerManager>,
    state: Arc<StateManager>,
) -> Result<u16, crate::core::error::Error> {
    Ok(*STREAM_PORT
        .get_or_try_init(|| async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .map_err(|e| crate::core::error::Error::Runtime(format!("streaming bind: {e}")))?;
            let port = listener
                .local_addr()
                .map_err(|e| crate::core::error::Error::Runtime(format!("streaming addr: {e}")))?
                .port();
            tokio::spawn(accept_loop(listener, exec, state));
            tracing::info!(port, "CRI streaming server listening");
            Ok::<u16, crate::core::error::Error>(port)
        })
        .await?)
}

async fn accept_loop(
    listener: tokio::net::TcpListener,
    exec: Arc<ContainerManager>,
    state: Arc<StateManager>,
) {
    loop {
        let Ok((tcp, _)) = listener.accept().await else {
            continue;
        };
        let exec = exec.clone();
        let state = state.clone();
        tokio::spawn(async move { handle_conn(tcp, exec, state).await });
    }
}

/// One SPDY connection = one exec/attach/portforward session. The upgrade
/// request line is where the client tells us which one: `/{mode}/{token}`.
async fn handle_conn(
    tcp: tokio::net::TcpStream,
    exec: Arc<ContainerManager>,
    state: Arc<StateManager>,
) {
    let (mut rd, mut wr) = tokio::io::split(tcp);
    let req = match parse_upgrade_request(&mut rd).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "streaming upgrade parse failed");
            return;
        }
    };
    let Some((mode, token)) = split_path(&req.path) else {
        tracing::warn!(path = %req.path, "streaming: bad path");
        return;
    };
    let session = {
        let mut g = sessions();
        match g.as_mut().and_then(|m| m.remove(token)) {
            Some(s) if mode_of(&s.kind) == mode => Arc::new(s),
            _ => {
                tracing::warn!(token = %token, mode = %mode, "streaming: unknown or expired session");
                return;
            }
        }
    };

    // The client rejects an upgrade that does not confirm the negotiated
    // subprotocol (kubectl-style negotiation; critest offers exactly these).
    let protocol = match session.kind {
        SessionKind::PortForward => "portforward.k8s.io",
        _ => "v4.channel.k8s.io",
    };
    if send_upgrade_response_with_protocol(&mut wr, Some(protocol))
        .await
        .is_err()
    {
        return;
    }
    let tcp = rd.unsplit(wr);

    let ctx = Arc::new(ConnCtx {
        exec_process: OnceCell::new(),
        pf_error_streams: Mutex::new(HashMap::new()),
        closed: watch::channel(false).0,
    });
    let sess = session.clone();
    tracing::info!(mode, %token, "streaming session opened");
    let ctx2 = ctx.clone();
    let exec2 = exec.clone();
    let state2 = state.clone();
    let conn = match Connection::serve(tcp, move |s: Arc<Stream>| {
        Box::pin(handle_stream(
            s,
            sess.clone(),
            ctx2.clone(),
            exec2.clone(),
            state2.clone(),
        ))
    })
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "streaming connection failed");
            return;
        }
    };

    // When the client goes away mid-exec the exec'd process would keep
    // running detached in the container; kill it (no-op after natural exit).
    tokio::spawn(async move {
        conn.wait_closed().await;
        ctx.closed.send_replace(true);
        tracing::info!(mode = mode_of(&session.kind), "streaming session closed");
        let handle = ctx.exec_process.get().filter(|_| matches!(session.kind, SessionKind::Exec));
        if let Some(Ok(h)) = handle {
            let pid = h.pid.load(Ordering::Acquire);
            if pid > 0 {
                tracing::info!(pid, "streaming: killing exec process on disconnect");
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
        }
    });
}

fn split_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.trim_start_matches('/');
    let mut parts = rest.splitn(3, '/');
    let mode = parts.next()?;
    let token = parts.next()?;
    if token.is_empty() || mode.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((mode, token))
}

fn mode_of(kind: &SessionKind) -> &'static str {
    match kind {
        SessionKind::Exec => "exec",
        SessionKind::Attach => "attach",
        SessionKind::PortForward => "portforward",
    }
}

/// Per-connection state shared by the stream handler tasks.
struct ConnCtx {
    /// The exec'd process, spawned by whichever channel stream arrives first.
    exec_process: OnceCell<Result<Arc<ExecHandle>, String>>,
    /// portforward's per-request error stream (client creates it first).
    pf_error_streams: Mutex<HashMap<(u16, String), Arc<Stream>>>,
    closed: watch::Sender<bool>,
}

/// Live handles to the exec'd process feeding the SPDY channel streams.
struct ExecHandle {
    _guest_lifetime: Option<oneshot::Sender<()>>,
    resize: Option<mpsc::Sender<(u16, u16)>>,
    pid: Arc<AtomicU32>,
    /// Output queues drained by the stdout/stderr channel tasks. The pumps
    /// start at spawn time, which can precede the matching SPDY stream.
    stdout_q: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    stderr_q: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    stdin_w: Mutex<Option<tokio::sync::mpsc::Sender<Vec<u8>>>>,
    exit: Mutex<Option<oneshot::Receiver<Result<i32, crate::core::error::Error>>>>,
}

async fn handle_stream(
    s: Arc<Stream>,
    session: Arc<Session>,
    ctx: Arc<ConnCtx>,
    exec: Arc<ContainerManager>,
    state: Arc<StateManager>,
) {
    let stream_type = s
        .headers
        .get("streamType")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    // Some clients open an untyped upgrade stream. Kubernetes client-go
    // uses stream 1 for its first typed channel, normally the exit status.
    if s.stream_id == 1 && stream_type.is_empty() {
        let _ = s.send_reply(HeaderMap::new(), true).await;
        return;
    }
    // client-go awaits the error stream reply before opening its data stream.
    // Publish the error stream first so a fast guest dial failure is observable.
    if matches!(session.kind, SessionKind::PortForward) && stream_type == "error" {
        let key = port_request(&s);
        let occupied = {
            use std::collections::hash_map::Entry;
            match ctx.pf_error_streams.lock().unwrap().entry(key.clone()) {
                Entry::Vacant(slot) => {
                    slot.insert(s.clone());
                    false
                }
                Entry::Occupied(_) => true,
            }
        };
        if occupied {
            let _ = s.reset(RstStatus::RefusedStream).await;
        } else if s.send_reply(HeaderMap::new(), false).await.is_err() {
            ctx.pf_error_streams.lock().unwrap().remove(&key);
        }
        return;
    }
    if s.send_reply(HeaderMap::new(), false).await.is_err() {
        return;
    }
    tracing::debug!(stream_id = s.stream_id, %stream_type, "streaming stream open");

    match session.kind {
        SessionKind::PortForward => {
            handle_pf_stream(s, stream_type, session, ctx, state, exec).await
        }
        _ => handle_exec_stream(s, stream_type, session, ctx, exec).await,
    }
}

async fn ensure_exec(
    ctx: &ConnCtx,
    session: &Session,
    exec: &ContainerManager,
) -> Result<Arc<ExecHandle>, String> {
    ctx.exec_process
        .get_or_init(|| spawn_exec(session, exec, ctx.closed.clone()))
        .await
        .clone()
}

/// Start an exec process or connect to the existing workload's stdio.
async fn spawn_exec(
    session: &Session,
    exec: &ContainerManager,
    closed: watch::Sender<bool>,
) -> Result<Arc<ExecHandle>, String> {
    let container = exec
        .get_container_state(&session.container_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "container no longer exists".to_owned())?;
    let options = StreamOptions {
        stdin: session.stdin,
        stdout: session.stdout,
        stderr: session.stderr,
        tty: session.tty,
    };
    let (streams, resize, lifetime) = if let Some(init) = &session.native_attach {
        let init = init.upgrade()
            .ok_or_else(|| "attach target instance is no longer available".to_owned())?;
        let stopped = container.status == crate::core::state::ContainerStatus::Stopped;
        let (streams, resize) = init.attach(options, stopped, closed).map_err(|e| e.to_string())?;
        (streams, resize, None)
    } else if container.sandbox_kind == "vm" {
        let op = match session.kind {
            SessionKind::Exec => Operation::ExecStream {
                argv: &session.cmd,
                stream: options,
            },
            SessionKind::Attach => Operation::Attach { stream: options },
            SessionKind::PortForward => return Err("unexpected port-forward session".into()),
        };
        let guest = exec
            .guest_stream(&container.pod_id, &container.id, op, options, closed.subscribe())
            .await
            .map_err(|e| e.to_string())?;
        (guest.io, guest.resize, Some(guest.lifetime))
    } else if matches!(session.kind, SessionKind::Exec) {
        let streams = exec
            .exec_stream(&session.container_id, &session.cmd, &[], None)
            .await
            .map_err(|e| e.to_string())?;

        (streams, None, None)
    } else {
        return Err("attach target has no init streams".into());
    };
    Ok(Arc::new(ExecHandle {
        _guest_lifetime: lifetime,
        resize,
        pid: streams.pid,
        stdout_q: Mutex::new(Some(streams.stdout)),
        stderr_q: Mutex::new(Some(streams.stderr)),
        stdin_w: Mutex::new(streams.stdin),
        exit: Mutex::new(Some(streams.exit)),
    }))
}

fn success_status() -> Bytes {
    Bytes::from(
        r#"{"kind":"Status","apiVersion":"v1","metadata":{},"status":"Success"}"#.to_string(),
    )
}

fn exit_code_status(code: i32) -> Bytes {
    Bytes::from(format!(
        r#"{{"kind":"Status","apiVersion":"v1","metadata":{{}},"status":"Failure","message":"command terminated with exit code {code}","reason":"NonZeroExitCode","details":{{"causes":[{{"reason":"ExitCode","message":"{code}"}}]}},"code":500}}"#
    ))
}

fn failure_status(msg: &str) -> Bytes {
    Bytes::from(
        serde_json::json!({"kind":"Status","apiVersion":"v1","metadata":{},
        "status":"Failure","message": msg,"code":500})
        .to_string(),
    )
}

async fn handle_exec_stream(
    s: Arc<Stream>,
    stream_type: String,
    session: Arc<Session>,
    ctx: Arc<ConnCtx>,
    exec: Arc<ContainerManager>,
) {
    match stream_type.as_str() {
        // Carries the metav1.Status once the process has been reaped.
        "error" => {
            let handle = match ensure_exec(&ctx, &session, &exec).await {
                Ok(h) => h,
                Err(e) => {
                    // The client decodes an EMPTY error stream as success, so
                    // this write is the only thing standing between a spawn
                    // failure and a silently "successful" session.
                    let r = s.write_data(failure_status(&e), true).await;
                    tracing::info!(error = %e, delivered = r.is_ok(), "streaming exec spawn failed: error stream notified");
                    return;
                }
            };
            let rx = handle.exit.lock().unwrap().take();
            let outcome = match rx {
                Some(rx) => rx.await.ok(),
                None => None,
            };
            tracing::info!(
                stream_id = s.stream_id,
                ?outcome,
                "exec error stream reporting exit"
            );
            let payload = match outcome {
                Some(Ok(0)) => success_status(),
                Some(Ok(code)) => exit_code_status(code),
                Some(Err(e)) => failure_status(&e.to_string()),
                // No exit report: the connection died or the backend worker
                // panicked. An empty error stream reads as success upstream,
                // so surface an explicit failure instead.
                None => failure_status("exec session ended without an exit status"),
            };
            let _ = s.write_data(payload, true).await;
        }
        "stdin" => {
            let handle = match ensure_exec(&ctx, &session, &exec).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::debug!(error = %e, "streaming exec spawn failed");
                    let _ = s.reset(RstStatus::RefusedStream).await;
                    return;
                }
            };
            let Some(tx) = handle.stdin_w.lock().unwrap().take() else {
                let _ = s.reset(RstStatus::RefusedStream).await;
                return;
            };
            let mut closed = ctx.closed.subscribe();
            let receive_input = async {
                while let Ok(Some(chunk)) = s.read_data().await {
                    if !chunk.is_empty() && tx.send(chunk.to_vec()).await.is_err() {
                        break;
                    }
                }
            };
            tokio::select! {
                _ = closed.wait_for(|closed| *closed) => {}
                _ = receive_input => {}
            }
            // Backend stdin ownership determines how client EOF is delivered.
        }
        "stdout" | "stderr" => {
            let handle = match ensure_exec(&ctx, &session, &exec).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::debug!(error = %e, "streaming exec spawn failed");
                    let _ = s.reset(RstStatus::RefusedStream).await;
                    return;
                }
            };
            let q = {
                let mut g = if stream_type == "stdout" {
                    handle.stdout_q.lock().unwrap()
                } else {
                    handle.stderr_q.lock().unwrap()
                };
                g.take()
            };
            let Some(mut q) = q else {
                let _ = s.reset(RstStatus::RefusedStream).await;
                return;
            };
            let mut closed = ctx.closed.subscribe();
            let send_output = async {
                while let Some(chunk) = q.recv().await {
                    if s.write_data(Bytes::from(chunk), false).await.is_err() {
                        return;
                    }
                }
            };
            tokio::select! {
                _ = closed.wait_for(|closed| *closed) => {}
                _ = send_output => {}
            }
            let _ = s.write_data(Bytes::new(), true).await;
        }
        "resize" if session.tty => {
            if let Ok(handle) = ensure_exec(&ctx, &session, &exec).await {
                if let Some(tx) = &handle.resize {
                    stream_resize(&s, tx).await;
                } else {
                    while let Ok(Some(_)) = s.read_data().await {}
                }
            }
        }
        _ => {
            let _ = s.reset(RstStatus::RefusedStream).await;
        }
    }
}

/// The Kubernetes resize stream is a sequence of JSON objects. SPDY DATA
/// frames may split one object or contain several.
async fn stream_resize(s: &Stream, tx: &mpsc::Sender<(u16, u16)>) {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Size {
        width: u16,
        height: u16,
    }
    let mut buffer = Vec::new();
    while let Ok(Some(data)) = s.read_data().await {
        buffer.extend_from_slice(&data);
        if buffer.len() > 64 * 1024 {
            break;
        }
        let mut items = serde_json::Deserializer::from_slice(&buffer).into_iter::<Size>();
        let mut consumed = 0;
        while let Some(item) = items.next() {
            match item {
                Ok(size) if size.width != 0 && size.height != 0 => {
                    consumed = items.byte_offset();
                    if tx.send((size.width, size.height)).await.is_err() {
                        return;
                    }
                }
                Err(e) if e.is_eof() => break,
                _ => return,
            }
        }
        buffer.drain(..consumed);
    }
}

fn port_request(s: &Stream) -> (u16, String) {
    let port = s
        .headers
        .get("port")
        .and_then(|s| s.to_str().ok())
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let key = s
        .headers
        .get("requestID")
        .and_then(|s| s.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| port.to_string());
    (port, key)
}

async fn handle_pf_stream(
    s: Arc<Stream>,
    stream_type: String,
    session: Arc<Session>,
    ctx: Arc<ConnCtx>,
    state: Arc<StateManager>,
    exec: Arc<ContainerManager>,
) {
    let key = port_request(&s);
    let port = key.0;
    if stream_type != "data" {
        let _ = s.reset(RstStatus::RefusedStream).await;
        return;
    }
    let Some(error) = ctx.pf_error_streams.lock().unwrap().remove(&key) else {
        let _ = s.reset(RstStatus::RefusedStream).await;
        return;
    };
    let result: Result<(), String> = async {
        if port == 0 || (!session.ports.is_empty() && !session.ports.contains(&port)) {
            return Err("port was not requested by CRI".into());
        }
        let pod = state
            .get_pod_state(&session.pod_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "Pod no longer exists".to_owned())?;
        if pod.runtime_handler == "vm" {
            let options = StreamOptions {
                stdin: true,
                stdout: true,
                ..Default::default()
            };
            let forward = async {
                let stream = exec
                    .guest_stream(
                        &pod.id,
                        &pod.id,
                        Operation::PortForward {
                            stream: PortOptions { port },
                        },
                        options,
                        ctx.closed.subscribe(),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                let _lifetime = stream.lifetime;
                relay_guest_port(s.clone(), stream.io).await
            };
            tokio::select! {
                result = forward => result,
                _ = s.wait_remote_reset() => Ok(()),
            }
        } else {
            let ip = if pod.network_ip.is_empty() {
                "127.0.0.1"
            } else {
                &pod.network_ip
            };
            let tcp = tokio::net::TcpStream::connect((ip, port))
                .await
                .map_err(|e| e.to_string())?;
            relay_pf(s.clone(), tcp).await;
            Ok(())
        }
    }
    .await;
    let msg = result.err().unwrap_or_default();
    let _ = error.write_data(Bytes::from(msg), true).await;
    let _ = s.write_data(Bytes::new(), true).await;
}

async fn relay_guest_port(
    s: Arc<Stream>,
    mut io: crate::sandbox::ExecStreams,
) -> Result<(), String> {
    let stdin = io
        .stdin
        .take()
        .ok_or_else(|| "guest port missing input".to_owned())?;
    let upstream = async {
        while let Some(data) = s.read_data().await.map_err(|e| e.to_string())? {
            stdin
                .send(data.to_vec())
                .await
                .map_err(|_| "guest port closed".to_owned())?;
        }
        drop(stdin);
        std::future::pending::<Result<(), String>>().await
    };
    let downstream = async {
        while let Some(chunk) = io.stdout.recv().await {
            s.write_data(Bytes::from(chunk), false)
                .await
                .map_err(|e| e.to_string())?;
        }
        let code = io
            .exit
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        if code == 0 {
            Ok(())
        } else {
            Err(format!("guest port returned {code}"))
        }
    };
    tokio::select! {r = upstream => r,r = downstream => r}
}

/// Bidirectional byte relay between the SPDY data stream and the container
/// port, until either side closes (the v1 portforward protocol is a pure
/// relay — no port echo on the data stream).
async fn relay_pf(s: Arc<Stream>, tcp: tokio::net::TcpStream) {
    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let s2 = s.clone();
    // Client → container: SPDY data frames out to the dialed socket.
    let up = tokio::spawn(async move {
        while let Ok(Some(data)) = s.read_data().await {
            if data.is_empty() {
                continue;
            }
            if tcp_write.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = tcp_write.shutdown().await;
    });
    // Container → client: socket reads become data frames.
    let down = tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match tcp_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if s2
                        .write_data(Bytes::copy_from_slice(&buf[..n]), false)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = s2.write_data(Bytes::new(), true).await;
    });
    let _ = up.await;
    let _ = down.await;
}

// ---------------------------------------------------------------------------
// CRI-side registration entry points (called from runtime_service)
// ---------------------------------------------------------------------------

/// Register an exec session and return the streaming URL.
pub async fn exec_url(
    exec: Arc<ContainerManager>,
    state: Arc<StateManager>,
    container_id: String,
    cmd: Vec<String>,
    tty: bool,
    stdin: bool,
    stdout: bool,
    stderr: bool,
) -> Result<String, crate::core::error::Error> {
    let port = ensure_started(exec.clone(), state).await?;
    let session = Session {
        kind: SessionKind::Exec,
        container_id,
        pod_id: String::new(),
        cmd,
        ports: Vec::new(),
        tty,
        stdin,
        stdout,
        stderr,
        native_attach: None,
    };
    let token = register(session);
    Ok(url_for(port, "exec", &token))
}

/// Register an attach session for the target container.
pub(crate) async fn attach_url(
    exec: Arc<ContainerManager>,
    state: Arc<StateManager>,
    container_id: String,
    options: StreamOptions,
    native_attach: Option<Weak<InitIo>>,
) -> Result<String, crate::core::error::Error> {
    let port = ensure_started(exec.clone(), state).await?;
    let session = Session {
        kind: SessionKind::Attach,
        container_id,
        pod_id: String::new(),
        cmd: Vec::new(),
        ports: Vec::new(),
        tty: options.tty,
        stdin: options.stdin,
        stdout: options.stdout,
        stderr: options.stderr,
        native_attach,
    };
    let token = register(session);
    Ok(url_for(port, "attach", &token))
}

/// Register a port-forward session for a pod sandbox.
pub async fn port_forward_url(
    exec: Arc<ContainerManager>,
    state: Arc<StateManager>,
    pod_id: String,
    ports: Vec<u16>,
) -> Result<String, crate::core::error::Error> {
    let port = ensure_started(exec, state).await?;
    let session = Session {
        kind: SessionKind::PortForward,
        container_id: String::new(),
        pod_id,
        cmd: Vec::new(),
        ports,
        tty: false,
        stdin: false,
        stdout: false,
        stderr: false,
        native_attach: None,
    };
    let token = register(session);
    Ok(url_for(port, "portforward", &token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotecommand_v4_reports_a_typed_process_exit() {
        let status: serde_json::Value = serde_json::from_slice(&exit_code_status(35)).unwrap();
        assert_eq!(status["reason"], "NonZeroExitCode");
        assert_eq!(
            status["details"]["causes"][0],
            serde_json::json!({"reason": "ExitCode", "message": "35"})
        );
    }
}
