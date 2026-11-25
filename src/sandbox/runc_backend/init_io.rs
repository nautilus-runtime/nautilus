use crate::core::{Error, Result};
use crate::sandbox::ExecStreams;
use crate::vm::workload::StreamOptions;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, watch};

const QUEUE_DEPTH: usize = 16;
const STARTUP_BYTES: usize = 16 * 1024;

type Bytes = Vec<u8>;
type Resize = mpsc::Sender<(u16, u16)>;

#[derive(Default)]
struct OutputState {
    startup: Option<Bytes>,
    subscribers: Vec<(mpsc::Sender<Bytes>, mpsc::Sender<()>)>,
    finished: bool,
}

pub(super) struct Output(Mutex<OutputState>);

impl Output {
    fn new() -> Self {
        Self(Mutex::new(OutputState {
            startup: Some(Vec::new()),
            ..Default::default()
        }))
    }

    fn subscribe(&self, enabled: bool, lagged: &mpsc::Sender<()>) -> mpsc::Receiver<Bytes> {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        if enabled {
            let mut output = self.0.lock().unwrap();
            if let Some(startup) = output.startup.take().filter(|b| !b.is_empty()) {
                let _ = tx.try_send(startup);
            }
            if !output.finished {
                output.subscribers.retain(|(tx, _)| !tx.is_closed());
                output.subscribers.push((tx, lagged.clone()));
            }
        }
        rx
    }

    pub(super) fn send(&self, bytes: &[u8]) {
        let mut output = self.0.lock().unwrap();
        if let Some(startup) = output.startup.as_mut() {
            let keep = bytes.len().min(STARTUP_BYTES - startup.len());
            startup.extend_from_slice(&bytes[..keep]);
        }
        output
            .subscribers
            .retain(|(tx, lagged)| match tx.try_send(bytes.to_vec()) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    let _ = lagged.try_send(());
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            });
    }

    pub(super) fn finish(&self) {
        let mut output = self.0.lock().unwrap();
        output.finished = true;
        output.subscribers.clear();
    }

    fn unsubscribe(&self, session: &mpsc::Sender<()>) {
        self.0
            .lock()
            .unwrap()
            .subscribers
            .retain(|(_, subscriber)| !subscriber.same_channel(session));
    }
}

#[derive(Default)]
struct Input {
    writer: Option<mpsc::Sender<Bytes>>,
    attached: bool,
}

/// Daemon-owned stdio for one native OCI init instance.
pub(crate) struct InitIo {
    pub(super) pid: u32,
    pub(super) stdout: Output,
    pub(super) stderr: Output,
    input: Mutex<Input>,
    terminal: Mutex<Option<Arc<File>>>,
    stdin_once: bool,
    exit: watch::Sender<Option<std::result::Result<i32, String>>>,
}

impl InitIo {
    pub(super) fn new(
        pid: u32,
        stdin: Option<Arc<File>>,
        terminal: Option<Arc<File>>,
        stdin_once: bool,
    ) -> Self {
        let tty = terminal.is_some();
        let writer = stdin.map(|file| {
            let (tx, mut rx) = mpsc::channel::<Bytes>(QUEUE_DEPTH);
            std::thread::spawn(move || {
                let mut file = &*file;
                while let Some(bytes) = rx.blocking_recv() {
                    if file.write_all(&bytes).is_err() {
                        return;
                    }
                }
                if tty {
                    send_terminal_eof(file);
                }
            });
            tx
        });
        Self {
            pid,
            stdout: Output::new(),
            stderr: Output::new(),
            input: Mutex::new(Input {
                writer,
                attached: false,
            }),
            terminal: Mutex::new(terminal),
            stdin_once,
            exit: watch::channel(None).0,
        }
    }

    pub(super) fn close_stdin(&self) {
        self.input.lock().unwrap().writer.take();
    }

    pub(crate) fn finish(&self, outcome: std::result::Result<i32, String>) {
        self.close_stdin();
        self.terminal.lock().unwrap().take();
        self.exit.send_replace(Some(outcome));
    }

    pub(crate) fn attach(
        self: Arc<Self>,
        options: StreamOptions,
        stopped: bool,
        cancel: watch::Sender<bool>,
    ) -> Result<(ExecStreams, Option<Resize>)> {
        let terminal = self.terminal.lock().unwrap().clone();
        let mut closed = cancel.subscribe();
        let stdin = if options.stdin {
            Some(self.attach_stdin(stopped, closed.clone())?)
        } else {
            None
        };
        let (lagged, mut lagged_rx) = mpsc::channel(1);
        let stdout = self.stdout.subscribe(options.stdout, &lagged);
        let stderr = self.stderr.subscribe(options.stderr, &lagged);
        let resize = terminal.map(|file| {
            let (tx, mut rx) = mpsc::channel(4);
            let mut closed = closed.clone();
            let mut exit = self.exit.subscribe();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = closed.wait_for(|closed| *closed) => break,
                        _ = exit.wait_for(Option::is_some) => break,
                        size = rx.recv() => match size {
                            Some((width, height)) => resize_terminal(&file, width, height),
                            None => break,
                        },
                    }
                }
            });
            tx
        });
        let (done, exit) = oneshot::channel();
        let mut native_exit = self.exit.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                Some(()) = lagged_rx.recv() => {
                    let _ = done.send(Err(Error::Runtime("attach client did not drain output".into())));
                    cancel.send_replace(true);
                    self.stdout.unsubscribe(&lagged);
                    self.stderr.unsubscribe(&lagged);
                }
                outcome = native_exit.wait_for(Option::is_some) => {
                    if let Ok(outcome) = outcome {
                        let _ = done.send(outcome.clone().unwrap().map_err(Error::Runtime));
                    }
                }
                _ = closed.wait_for(|closed| *closed) => {
                    self.stdout.unsubscribe(&lagged);
                    self.stderr.unsubscribe(&lagged);
                }
            }
        });
        Ok((
            ExecStreams {
                stdin,
                stdout,
                stderr,
                pid: Default::default(),
                exit,
            },
            resize,
        ))
    }

    fn attach_stdin(
        self: &Arc<Self>,
        stopped: bool,
        mut closed: watch::Receiver<bool>,
    ) -> Result<mpsc::Sender<Bytes>> {
        let mut input = self.input.lock().unwrap();
        if stopped || self.exit.borrow().is_some() {
            // A short-lived init may exit before the client opens its streams.
            // Accept the stdin stream while the client drains output and exit.
            return Ok(mpsc::channel(1).0);
        }
        if input.attached {
            return Err(Error::Runtime(
                "container stdin already has an attach client".into(),
            ));
        }
        let writer = input
            .writer
            .clone()
            .ok_or_else(|| Error::Runtime("container stdin is closed".into()))?;
        input.attached = true;
        let (tx, mut rx) = mpsc::channel(QUEUE_DEPTH);
        let io = self.clone();
        let mut exit = self.exit.subscribe();
        tokio::spawn(async move {
            let forward = async {
                while let Some(bytes) = rx.recv().await {
                    if writer.send(bytes).await.is_err() {
                        break;
                    }
                }
            };
            tokio::select! {
                _ = closed.wait_for(|closed| *closed) => {}
                _ = exit.wait_for(Option::is_some) => {}
                _ = forward => {}
            }
            let mut input = io.input.lock().unwrap();
            input.attached = false;
            if io.stdin_once {
                input.writer.take();
            }
        });
        Ok(tx)
    }
}

fn send_terminal_eof(mut file: &File) {
    // A PTY has one bidirectional master. Preserve output while delivering the
    // terminal's configured EOF character to a canonical-mode reader.
    let mut term = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(file.as_raw_fd(), term.as_mut_ptr()) } == 0 {
        let term = unsafe { term.assume_init() };
        let eof = term.c_cc[libc::VEOF];
        if term.c_lflag & libc::ICANON != 0 && eof != 0 {
            let _ = file.write_all(&[eof]);
        }
    }
}

fn resize_terminal(file: &File, width: u16, height: u16) {
    let size = libc::winsize {
        ws_row: height,
        ws_col: width,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCSWINSZ, &size) } == -1 {
        tracing::debug!(error = %std::io::Error::last_os_error(), "init PTY resize failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_is_bounded_and_only_replayed_to_the_first_output_client() {
        let output = Output::new();
        let (lagged, mut failures) = mpsc::channel(1);
        output.send(&vec![b'a'; STARTUP_BYTES + 1]);
        let mut first = output.subscribe(true, &lagged);
        assert_eq!(first.recv().await.unwrap(), vec![b'a'; STARTUP_BYTES]);
        let mut second = output.subscribe(true, &lagged);
        assert!(second.try_recv().is_err());
        for _ in 0..=QUEUE_DEPTH {
            output.send(b"live");
            assert_eq!(second.recv().await.unwrap(), b"live");
        }
        assert_eq!(failures.recv().await, Some(()));
        output.finish();
        assert!(second.recv().await.is_none());
    }

    #[tokio::test]
    async fn overflowing_stdout_releases_the_whole_attach_session() {
        let init = Arc::new(InitIo::new(42, None, None, false));
        let (cancel, mut closed) = watch::channel(false);
        let (mut streams, _) = init
            .clone()
            .attach(
                StreamOptions {
                    stdout: true,
                    stderr: true,
                    ..Default::default()
                },
                false,
                cancel,
            )
            .unwrap();
        for _ in 0..=QUEUE_DEPTH {
            init.stdout.send(b"stdout");
        }
        assert!(streams.exit.await.unwrap().is_err());
        assert!(closed.wait_for(|closed| *closed).await.is_ok());
        assert!(streams.stderr.recv().await.is_none());
    }

    #[tokio::test]
    async fn attach_to_a_fast_exit_can_drain_output_and_status() {
        let init = Arc::new(InitIo::new(42, None, None, true));
        init.stdout.send(b"first line");
        init.stdout.finish();
        init.finish(Ok(37));
        let (cancel, _) = watch::channel(false);
        let (mut streams, _) = init
            .attach(
                StreamOptions {
                    stdin: true,
                    stdout: true,
                    ..Default::default()
                },
                true,
                cancel,
            )
            .unwrap();
        assert!(streams.stdin.unwrap().is_closed());
        assert_eq!(streams.stdout.recv().await.unwrap(), b"first line");
        assert!(streams.stdout.recv().await.is_none());
        assert_eq!(streams.exit.await.unwrap().unwrap(), 37);
    }
}
