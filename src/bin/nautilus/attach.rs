use std::io;
use std::net::Ipv4Addr;
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;

use bytes::Bytes;
use spdystream_rs::frame::{DataFrame, SynStreamFrame, DATA_FLAG_FIN, FLAG_FIN};
use spdystream_rs::{framer::Framer, Frame};
use tokio::io::{unix::AsyncFd, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

const PROTOCOL: &str = "v4.channel.k8s.io";
const STATUS: u32 = 1;
const STDOUT: u32 = 3;
const STDERR: u32 = 5;
const STDIN: u32 = 7;
const RESIZE: u32 = 9;
const MAX_STATUS_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Options {
    pub stdin: bool,
    pub tty: bool,
}

impl Options {
    pub fn enabled(self) -> bool {
        self.stdin || self.tty
    }

    pub fn validate(self) -> Result<(), String> {
        if self.stdin && self.tty && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return Err("the input device is not a terminal".into());
        }
        Ok(())
    }
}

pub(super) async fn stream(url: &str, options: Options) -> Result<u8, String> {
    let (reader, writer) = tokio::time::timeout(Duration::from_secs(10), connect(url))
        .await
        .map_err(|_| "attach upgrade timed out".to_owned())??;
    let mut reader = Framer::new(tokio::io::sink(), reader);
    let mut writer = Framer::new(writer, tokio::io::empty());
    let channels = [
        (STATUS, "error", true),
        (STDOUT, "stdout", true),
        (STDERR, "stderr", !options.tty),
        (STDIN, "stdin", options.stdin),
        (RESIZE, "resize", options.tty),
    ];
    for (stream_id, channel, enabled) in channels {
        if enabled {
            let mut headers = http::HeaderMap::new();
            headers.insert("streamtype", http::HeaderValue::from_static(channel));
            writer
                .write_frame(&Frame::SynStream(SynStreamFrame {
                    stream_id,
                    associated_stream_id: 0,
                    priority: 0,
                    headers,
                    flags: 0,
                }))
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    // Open nonblocking input instead of tokio::io::stdin, whose uncancellable
    // read can keep the runtime alive after the remote command exits.
    let stdin = options
        .stdin
        .then(Input::open)
        .transpose()
        .map_err(|e| e.to_string())?;
    let _terminal = (options.stdin && options.tty)
        .then(RawTerminal::enter)
        .transpose()
        .map_err(|e| format!("configure terminal: {e}"))?;
    let (control, frames) = mpsc::channel(4);
    let send = send_input(&mut writer, stdin, options, frames);
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let receive = receive_output(&mut reader, options, &mut stdout, &mut stderr, control);
    tokio::pin!(send, receive);
    tokio::select! {
        biased;
        result = &mut receive => result,
        result = &mut send => result,
    }
}

async fn connect(
    url: &str,
) -> Result<
    (
        BufReader<tokio::net::tcp::OwnedReadHalf>,
        tokio::net::tcp::OwnedWriteHalf,
    ),
    String,
> {
    // The runtime generates a local HTTP endpoint. Reject credentials,
    // redirects and nonlocal addresses before sending its one-use token.
    let uri: http::Uri = url.parse().map_err(|_| "invalid attach URL")?;
    let port = uri
        .port_u16()
        .filter(|port| *port != 0)
        .ok_or("invalid attach port")?;
    if uri.scheme_str() != Some("http")
        || uri.authority().map(|a| a.as_str()) != Some(format!("127.0.0.1:{port}").as_str())
        || !uri.path().starts_with("/attach/")
    {
        return Err("attach URL must use the local runtime streaming endpoint".into());
    }
    let path = uri.path_and_query().ok_or("invalid attach URL path")?;
    let (reader, mut writer) = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|e| format!("connect to attach endpoint: {e}"))?
        .into_split();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: Upgrade\r\nUpgrade: SPDY/3.1\r\nX-Stream-Protocol-Version: {PROTOCOL}\r\n\r\n"
    );
    writer
        .write_all(request.as_bytes())
        .await
        .map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(reader);
    let mut header = Vec::new();
    loop {
        header.push(
            reader
                .read_u8()
                .await
                .map_err(|e| format!("attach upgrade: {e}"))?,
        );
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        if header.len() >= 8192 {
            return Err("attach upgrade headers are too large".into());
        }
    }
    let response = std::str::from_utf8(&header).map_err(|_| "invalid attach upgrade response")?;
    let mut lines = response.split("\r\n");
    if lines.next().and_then(|line| line.split_whitespace().nth(1)) != Some("101") {
        return Err("runtime rejected the attach upgrade".into());
    }
    let has_header = |name: &str, value: &str| {
        lines
            .clone()
            .filter_map(|line| line.split_once(':'))
            .any(|(key, candidate)| {
                key.trim().eq_ignore_ascii_case(name)
                    && candidate.trim().eq_ignore_ascii_case(value)
            })
    };
    if !has_header("upgrade", "spdy/3.1") || !has_header("x-stream-protocol-version", PROTOCOL) {
        return Err("runtime did not negotiate v4 SPDY attach".into());
    }
    Ok((reader, writer))
}

async fn send_input<W: AsyncWrite + Unpin>(
    writer: &mut Framer<W, tokio::io::Empty>,
    mut stdin: Option<Input>,
    options: Options,
    mut control: mpsc::Receiver<Frame>,
) -> Result<u8, String> {
    let mut window_change = options
        .tty
        .then(|| signal(SignalKind::window_change()))
        .transpose()
        .map_err(|e| e.to_string())?;
    if options.tty {
        send_window_size(writer).await?;
    }
    let mut buffer = [0; 8192];
    loop {
        let frame = tokio::select! {
            Some(frame) = control.recv() => frame,
            result = async { stdin.as_mut().unwrap().read(&mut buffer).await }, if stdin.is_some() => {
                let size = result.map_err(|e| format!("read stdin: {e}"))?;
                if size == 0 {
                    stdin = None;
                }
                data_frame(STDIN, Bytes::copy_from_slice(&buffer[..size]), size == 0)
            }
            Some(()) = async { window_change.as_mut().unwrap().recv().await }, if window_change.is_some() => {
                send_window_size(writer).await?;
                continue;
            }
        };
        writer
            .write_frame(&frame)
            .await
            .map_err(|e| e.to_string())?;
    }
}

fn data_frame(stream_id: u32, data: Bytes, fin: bool) -> Frame {
    Frame::Data(DataFrame {
        stream_id,
        flags: if fin { DATA_FLAG_FIN } else { 0 },
        data,
    })
}

async fn send_window_size<W: AsyncWrite + Unpin>(
    writer: &mut Framer<W, tokio::io::Empty>,
) -> Result<(), String> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO] {
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0
            && size.ws_row != 0
            && size.ws_col != 0
        {
            let payload = format!(r#"{{"Width":{},"Height":{}}}"#, size.ws_col, size.ws_row);
            writer
                .write_frame(&data_frame(RESIZE, Bytes::from(payload), false))
                .await
                .map_err(|e| e.to_string())?;
            break;
        }
    }
    Ok(())
}

async fn receive_output<R: AsyncRead + Unpin>(
    reader: &mut Framer<tokio::io::Sink, R>,
    options: Options,
    stdout: &mut (impl AsyncWrite + Unpin),
    stderr: &mut (impl AsyncWrite + Unpin),
    control: mpsc::Sender<Frame>,
) -> Result<u8, String> {
    let mut status = Vec::new();
    let mut finished = [false, false, options.tty];
    let mut reset_output = None;
    while !finished.iter().all(|done| *done) {
        let frame = reader
            .read_frame()
            .await
            .map_err(|e| format!("read attach stream: {e}"))?;
        let final_stream = match frame {
            Frame::Data(frame) => {
                match frame.stream_id {
                    STATUS => {
                        if status.len() + frame.data.len() > MAX_STATUS_BYTES {
                            return Err("attach status is too large".into());
                        }
                        status.extend_from_slice(&frame.data);
                    }
                    STDOUT => stdout
                        .write_all(&frame.data)
                        .await
                        .map_err(|e| e.to_string())?,
                    STDERR if !options.tty => stderr
                        .write_all(&frame.data)
                        .await
                        .map_err(|e| e.to_string())?,
                    _ => {}
                }
                (frame.flags & DATA_FLAG_FIN != 0).then_some(frame.stream_id)
            }
            Frame::SynReply(frame) => (frame.flags & FLAG_FIN != 0).then_some(frame.stream_id),
            Frame::Headers(frame) => (frame.flags & FLAG_FIN != 0).then_some(frame.stream_id),
            Frame::RstStream(frame) => {
                if frame.stream_id == STATUS {
                    return Err(format!("attach status stream reset: {:?}", frame.status));
                }
                if matches!(frame.stream_id, STDOUT | STDERR) {
                    reset_output = Some(frame.status);
                }
                Some(frame.stream_id)
            }
            Frame::Ping(frame) if frame.id % 2 == 0 => {
                control
                    .send(Frame::Ping(frame))
                    .await
                    .map_err(|e| e.to_string())?;
                None
            }
            Frame::GoAway(frame) => {
                return Err(format!("attach connection closed: {:?}", frame.status))
            }
            _ => None,
        };
        match final_stream {
            Some(STATUS) => finished[0] = true,
            Some(STDOUT) => finished[1] = true,
            Some(STDERR) => finished[2] = true,
            _ => {}
        }
    }
    stdout.flush().await.map_err(|e| e.to_string())?;
    stderr.flush().await.map_err(|e| e.to_string())?;
    let code = parse_status(&status)?;
    if let Some(reset) = reset_output {
        return Err(format!("attach output stream reset: {reset:?}"));
    }
    Ok(code)
}

fn parse_status(bytes: &[u8]) -> Result<u8, String> {
    #[derive(serde::Deserialize)]
    struct Status {
        status: String,
        message: Option<String>,
        reason: Option<String>,
        details: Option<Details>,
    }
    #[derive(serde::Deserialize)]
    struct Details {
        causes: Vec<Cause>,
    }
    #[derive(serde::Deserialize)]
    struct Cause {
        reason: String,
        message: String,
    }
    if bytes.is_empty() {
        return Ok(0); // client-go treats an empty v4 error channel as success.
    }
    let status: Status =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid attach status: {e}"))?;
    if status.status == "Success" {
        return Ok(0);
    }
    if status.reason.as_deref() == Some("NonZeroExitCode") {
        let details = status
            .details
            .ok_or("attach status is missing exit details")?;
        let exit = details
            .causes
            .iter()
            .find(|cause| cause.reason == "ExitCode")
            .ok_or("attach status is missing an exit code")?;
        return exit
            .message
            .parse::<u8>()
            .map_err(|_| "attach status contains an invalid exit code".into());
    }
    Err(status.message.unwrap_or_else(|| "attach failed".into()))
}

enum Input {
    Pollable(AsyncFd<std::fs::File>),
    File(tokio::fs::File),
}

impl Input {
    fn open() -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/proc/self/fd/0")?;
        match AsyncFd::new(file.try_clone()?) {
            Ok(fd) => Ok(Self::Pollable(fd)),
            // Linux epoll rejects regular files and devices like /dev/null.
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
                Ok(Self::File(tokio::fs::File::from_std(file)))
            }
            Err(e) => Err(e),
        }
    }

    async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        use std::io::Read;
        match self {
            Self::File(file) => file.read(buffer).await,
            Self::Pollable(fd) => loop {
                let mut ready = fd.readable().await?;
                match ready.try_io(|inner| inner.get_ref().read(buffer)) {
                    Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                    Ok(result) => return result,
                    Err(_) => {}
                }
            },
        }
    }
}

struct RawTerminal(libc::termios);

impl RawTerminal {
    fn enter() -> io::Result<Self> {
        let mut current = std::mem::MaybeUninit::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, current.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let current = unsafe { current.assume_init() };
        let mut raw = current;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(current))
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drains_both_outputs_after_the_exit_status() {
        let (server, client) = tokio::io::duplex(1024);
        let mut sender = Framer::new(server, tokio::io::empty());
        let mut reader = Framer::new(tokio::io::sink(), client);
        let (control, _) = mpsc::channel(1);
        let payload = Bytes::from_static(
            br#"{"status":"Failure","reason":"NonZeroExitCode","details":{"causes":[{"reason":"ExitCode","message":"37"}]}}"#,
        );
        let write = async {
            for frame in [
                data_frame(STATUS, payload, true),
                data_frame(STDERR, Bytes::from_static(b"stderr"), true),
                data_frame(STDOUT, Bytes::from_static(b"stdout"), true),
            ] {
                sender.write_frame(&frame).await.unwrap();
            }
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let (result, ()) = tokio::join!(
            receive_output(
                &mut reader,
                Options::default(),
                &mut stdout,
                &mut stderr,
                control
            ),
            write,
        );
        assert_eq!(result.unwrap(), 37);
        assert_eq!(stdout, b"stdout");
        assert_eq!(stderr, b"stderr");
    }

    #[tokio::test]
    async fn upgrades_and_opens_the_expected_channels() {
        use spdystream_rs::server::{parse_upgrade_request, send_upgrade_response_with_protocol};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let url = format!("http://{}/attach/token", listener.local_addr().unwrap());
        let server = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = parse_upgrade_request(&mut socket).await.unwrap();
            assert_eq!(request.path, "/attach/token");
            assert_eq!(request.headers["x-stream-protocol-version"], PROTOCOL);
            send_upgrade_response_with_protocol(&mut socket, Some(PROTOCOL))
                .await
                .unwrap();
            let (reader, writer) = socket.into_split();
            let mut framer = Framer::new(writer, reader);
            for (stream_id, channel) in [(STATUS, "error"), (STDOUT, "stdout"), (STDERR, "stderr")]
            {
                let Frame::SynStream(frame) = framer.read_frame().await.unwrap() else {
                    panic!("expected SYN_STREAM");
                };
                assert_eq!(frame.stream_id, stream_id);
                assert_eq!(frame.headers["streamtype"], channel);
            }
            for id in [STDOUT, STATUS, STDERR] {
                framer
                    .write_frame(&data_frame(id, Bytes::new(), true))
                    .await
                    .unwrap();
            }
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(stream(&url, Options::default()), server)
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap(), 0);
    }
}
