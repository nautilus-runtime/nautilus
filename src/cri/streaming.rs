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
//! in the stream handler closure; the request stream (stream 1) carries no
//! `streamType` and is only acked.

use crate::core::state::StateManager;
use crate::runtime::container::ContainerManager;
use bytes::Bytes;
use http::HeaderMap;
use spdystream_rs::connection::Connection;
use spdystream_rs::frame::RstStatus;
use spdystream_rs::server::{parse_upgrade_request, send_upgrade_response_with_protocol};
use spdystream_rs::stream::Stream;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, OnceCell};

/// What the client asked for at CRI RPC time.
#[derive(Clone, Debug)]
pub enum SessionKind {
    Exec,
    /// CRI attach attaches to the container's primary process; the workload
    /// stdio here is file-based (no pipes survive a detached start), so a
    /// session serves a fresh `/bin/sh` in the container namespaces instead.
    /// Semantically an approximation, streaming-plumbing-wise identical to
    /// exec.
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
    pub tty: bool,
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
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
    g.get_or_insert_with(HashMap::new).insert(token.clone(), session);
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
        let Ok((tcp, _)) = listener.accept().await else { continue };
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
        pf_error_stream: Mutex::new(None),
    });
    let sess = session.clone();
    tracing::info!(mode, %token, "streaming session opened");
    let ctx2 = ctx.clone();
    let exec2 = exec.clone();
    let state2 = state.clone();
    let conn = match Connection::serve(tcp, move |s: Arc<Stream>| {
        Box::pin(handle_stream(s, sess.clone(), ctx2.clone(), exec2.clone(), state2.clone()))
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
        tracing::info!(mode = mode_of(&session.kind), "streaming session closed");
        if let Some(h) = ctx.exec_process.get() {
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
    exec_process: OnceCell<Arc<ExecHandle>>,
    /// portforward's per-request error stream (client creates it first).
    pf_error_stream: Mutex<Option<Arc<Stream>>>,
}

/// Live handles to the exec'd process feeding the SPDY channel streams.
struct ExecHandle {
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
    // Stream 1 is the HTTP upgrade request itself (client-go creates it via
    // CreateStream(req.Header)); it carries no channel data. Ack it so the
    // client's upgrade RoundTrip completes, then drop it.
    if s.stream_id == 1 {
        let _ = s.send_reply(HeaderMap::new(), true).await;
        return;
    }
    if s.send_reply(HeaderMap::new(), false).await.is_err() {
        return;
    }
    let stream_type = s
        .headers
        .get("streamType")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    tracing::debug!(stream_id = s.stream_id, %stream_type, "streaming stream open");

    match session.kind {
        SessionKind::PortForward => handle_pf_stream(s, stream_type, session, ctx, state).await,
        _ => handle_exec_stream(s, stream_type, session, ctx, exec).await,
    }
}

async fn ensure_exec(
    ctx: &ConnCtx,
    session: &Session,
    exec: &ContainerManager,
) -> Result<Arc<ExecHandle>, String> {
    ctx.exec_process
        .get_or_try_init(|| spawn_exec(session, exec))
        .await
        .map(Arc::clone)
}

/// Spawn the container process behind an exec/attach session: one process per
/// connection, socketpair stdio, reaped by the backend's blocking worker.
async fn spawn_exec(
    session: &Session,
    exec: &ContainerManager,
) -> Result<Arc<ExecHandle>, String> {
    let streams = exec
        .exec_stream(&session.container_id, &session.cmd, &[], None)
        .await
        .map_err(|e| e.to_string())?;

    Ok(Arc::new(ExecHandle {
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
        r#"{{"kind":"Status","apiVersion":"v1","metadata":{{}},"status":"Failure","message":"command terminated with exit code {code}","reason":"NonZeroExitCodeError","details":{{"causes":[{{"reason":"ExitCode","message":"{code}"}}]}},"code":500}}"#
    ))
}

fn failure_status(msg: &str) -> Bytes {
    // Minimal JSON string escaping; messages are our own runtime errors.
    let escaped = msg.replace('\\', "\\\\").replace('"', "\\\"");
    Bytes::from(format!(
        r#"{{"kind":"Status","apiVersion":"v1","metadata":{{}},"status":"Failure","message":"{escaped}","code":500}}"#
    ))
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
            tracing::info!(stream_id = s.stream_id, ?outcome, "exec error stream reporting exit");
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
            loop {
                match s.read_data().await {
                    Ok(Some(chunk)) => {
                        if chunk.is_empty() {
                            continue;
                        }
                        // Dropping `tx` (on client EOF below) closes the
                        // process's stdin via the writer thread.
                        if tx.send(chunk.to_vec()).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        tracing::info!(stream_id = s.stream_id, "streaming stdin: client FIN");
                        break;
                    }
                    Err(e) => {
                        tracing::info!(stream_id = s.stream_id, error = %e, "streaming stdin: read error");
                        break;
                    }
                }
            }
            // Dropping `tx` EOFs the process stdin. That is the session-end
            // contract for BOTH exec and attach: client-go's executor only
            // returns once stdout/stderr EOF, which for a shell means its
            // stdin must close. (Keeping stdin open parks the session
            // forever.)
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
            while let Some(chunk) = q.recv().await {
                if s.write_data(Bytes::from(chunk), false).await.is_err() {
                    break;
                }
            }
            tracing::debug!(stream_id = s.stream_id, %stream_type, "output channel closed, sending FIN");
            let _ = s.write_data(Bytes::new(), true).await;
        }
        // Terminal resize: needs a pty (not wired); drain and ignore so the
        // tty-mode client is not disturbed.
        "resize" => {
            while let Ok(Some(_)) = s.read_data().await {}
        }
        _ => {
            let _ = s.reset(RstStatus::RefusedStream).await;
        }
    }
}

async fn handle_pf_stream(
    s: Arc<Stream>,
    stream_type: String,
    session: Arc<Session>,
    ctx: Arc<ConnCtx>,
    state: Arc<StateManager>,
) {
    match stream_type.as_str() {
        // Per-request error channel: remember it so data-stream dial failures
        // surface as protocol errors instead of a silent hang.
        "error" => {
            *ctx.pf_error_stream.lock().unwrap() = Some(s);
        }
        "data" => {
            let port: u16 = s
                .headers
                .get("port")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if port == 0 {
                report_pf_error(&ctx, "port-forward: missing or invalid port header").await;
                return;
            }
            // Dial the pod IP from the host; the bridge network is host-routable.
            // Host-network pods have no CNI IP — they listen on the node itself.
            let ip = match state.get_pod_state(&session.pod_id).await {
                Ok(Some(pod)) if !pod.network_ip.is_empty() => pod.network_ip,
                _ => "127.0.0.1".to_string(),
            };
            match tokio::net::TcpStream::connect((ip.as_str(), port)).await {
                Ok(tcp) => relay_pf(s, tcp).await,
                Err(e) => {
                    tracing::debug!(%ip, port, error = %e, "port-forward dial failed");
                    report_pf_error(&ctx, &format!("error forwarding port {port}: {e}")).await;
                }
            }
        }
        _ => {
            let _ = s.reset(RstStatus::RefusedStream).await;
        }
    }
}

async fn report_pf_error(ctx: &ConnCtx, msg: &str) {
    let stream = ctx.pf_error_stream.lock().unwrap().clone();
    if let Some(es) = stream {
        let _ = es.write_data(failure_status(msg), true).await;
    }
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
        tty,
        stdin,
        stdout,
        stderr,
    };
    let token = register(session);
    Ok(url_for(port, "exec", &token))
}

/// Register an attach session (served as a fresh /bin/sh; see SessionKind).
pub async fn attach_url(
    exec: Arc<ContainerManager>,
    state: Arc<StateManager>,
    container_id: String,
    tty: bool,
    stdin: bool,
    stdout: bool,
    stderr: bool,
) -> Result<String, crate::core::error::Error> {
    let port = ensure_started(exec.clone(), state).await?;
    let session = Session {
        kind: SessionKind::Attach,
        container_id,
        pod_id: String::new(),
        cmd: vec!["/bin/sh".to_string()],
        tty,
        stdin,
        stdout,
        stderr,
    };
    let token = register(session);
    Ok(url_for(port, "attach", &token))
}

/// Register a port-forward session for a pod sandbox.
pub async fn port_forward_url(
    exec: Arc<ContainerManager>,
    state: Arc<StateManager>,
    pod_id: String,
) -> Result<String, crate::core::error::Error> {
    let port = ensure_started(exec, state).await?;
    let session = Session {
        kind: SessionKind::PortForward,
        container_id: String::new(),
        pod_id,
        cmd: Vec::new(),
        tty: false,
        stdin: false,
        stdout: false,
        stderr: false,
    };
    let token = register(session);
    Ok(url_for(port, "portforward", &token))
}
