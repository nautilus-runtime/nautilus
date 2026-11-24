//! Binary stdio frames following the workload agent's authenticated stream reply.
use super::workload::{Channel, Operation, StreamOptions};
use crate::core::Result;
use crate::sandbox::ExecStreams;
use std::{future::pending, io, sync::Arc};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};

const MAX: usize = 64 * 1024;

pub(crate) struct GuestStream {
    pub io: ExecStreams,
    pub lifetime: oneshot::Sender<()>,
    pub resize: Option<mpsc::Sender<(u16, u16)>>,
}

pub(crate) async fn open(
    channel: Channel,
    id: &str,
    op: Operation<'_>,
    options: StreamOptions,
    mut disconnected: watch::Receiver<bool>,
) -> Result<GuestStream> {
    let socket = tokio::select! {
        r = channel.open_stream(id, op) => r?,
        _ = disconnected.wait_for(|v| *v) => return Err(io::Error::from(io::ErrorKind::ConnectionAborted).into()),
    };
    let (rd, wr) = tokio::io::split(socket);
    let (stdin_tx, stdin_rx) = mpsc::channel(8);
    let (resize_tx, resize_rx) = mpsc::channel(8);
    let (stdout_tx, stdout_rx) = mpsc::channel(8);
    let (stderr_tx, stderr_rx) = mpsc::channel(8);
    let (exit_tx, exit_rx) = oneshot::channel();
    let (lifetime, dropped) = oneshot::channel::<()>();
    tokio::spawn(async move {
        // Both halves are dropped together on disconnect, including when
        // backpressure on an SPDY output channel has paused guest reads.
        let result = tokio::select! {
            r = outputs(rd, stdout_tx, stderr_tx, options) => r,
            r = inputs(wr, stdin_rx, resize_rx, options) => r,
            _ = dropped => Err(io::Error::from(io::ErrorKind::ConnectionAborted)),
            _ = disconnected.wait_for(|v| *v) => Err(io::Error::from(io::ErrorKind::ConnectionAborted)),
        };
        let _ = exit_tx.send(result.map_err(Into::into));
    });
    Ok(GuestStream {
        lifetime,
        io: ExecStreams {
            pid: Arc::default(),
            stdin: options.stdin.then_some(stdin_tx),
            stdout: stdout_rx,
            stderr: stderr_rx,
            exit: exit_rx,
        },
        resize: options.tty.then_some(resize_tx),
    })
}

async fn inputs(
    mut wr: impl AsyncWrite + Unpin,
    mut input: mpsc::Receiver<Vec<u8>>,
    mut resize: mpsc::Receiver<(u16, u16)>,
    options: StreamOptions,
) -> io::Result<i32> {
    let mut input_open = options.stdin;
    let mut resize_open = options.tty;
    loop {
        tokio::select! {
            chunk = input.recv(), if input_open => match chunk {
                Some(bytes) => for part in bytes.chunks(MAX) { send(&mut wr, 0, part).await?; },
                None => { send(&mut wr, 5, &[]).await?; input_open = false; }
            },
            size = resize.recv(), if resize_open => match size {
                Some((w,h)) => { let [a,b]=w.to_be_bytes(); let [c,d]=h.to_be_bytes(); send(&mut wr,4,&[a,b,c,d]).await?; },
                None => resize_open = false,
            },
            else => return pending().await,
        }
    }
}

async fn send(wr: &mut (impl AsyncWrite + Unpin), tag: u8, payload: &[u8]) -> io::Result<()> {
    wr.write_u8(tag).await?;
    wr.write_u32(payload.len() as u32).await?;
    wr.write_all(payload).await
}

async fn outputs(
    mut rd: impl AsyncRead + Unpin,
    stdout: mpsc::Sender<Vec<u8>>,
    stderr: mpsc::Sender<Vec<u8>>,
    options: StreamOptions,
) -> io::Result<i32> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "unexpected VM stream frame");
    loop {
        let tag = rd.read_u8().await?;
        let length = rd.read_u32().await? as usize;
        if length > MAX || !matches!(tag, 1 | 2 | 3 | 6) || (tag == 6 && length != 4) {
            return Err(invalid());
        }
        let mut bytes = vec![0; length];
        rd.read_exact(&mut bytes).await?;
        match tag {
            6 => return Ok(i32::from_be_bytes(bytes.try_into().map_err(|_| invalid())?)),
            3 => {
                return Err(io::Error::other(
                    String::from_utf8_lossy(&bytes).into_owned(),
                ))
            }
            1 if options.stdout => {
                stdout
                    .send(bytes)
                    .await
                    .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            }
            2 if options.stderr && !options.tty => {
                stderr
                    .send(bytes)
                    .await
                    .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            }
            _ => return Err(invalid()),
        }
    }
}
