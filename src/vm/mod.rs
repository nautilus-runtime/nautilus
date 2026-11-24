//! Local Cloud Hypervisor compatibility probe.

#[cfg(target_os = "linux")]
pub mod owner;

pub mod resources;

pub mod guest;
pub(crate) mod workload;
pub(crate) mod stream;

use std::io::{self, ErrorKind};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::{sleep, timeout};

/// Boot one guest, check its agent, then reap the VMM before removing its sockets.
pub async fn probe(vmm: &Path, kernel: &Path, initramfs: &Path) -> io::Result<()> {
    // The VMM runs in a private directory. Resolve caller-relative paths first.
    let resolve = |path: &Path| {
        path.canonicalize()
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))
    };
    let mut command = Command::new(resolve(vmm)?);
    command
        .arg("--kernel")
        .arg(resolve(kernel)?)
        .arg("--initramfs")
        .arg(resolve(initramfs)?)
        .args(["--cpus", "boot=1", "--memory", "size=384M"])
        .args(["--cmdline", "console=ttyS0 rdinit=/init panic=1"])
        .args(["--console", "off", "--serial", "off"])
        .args([
            "--api-socket",
            "api.sock",
            "--vsock",
            "cid=3,socket=vsock.sock",
        ]);
    let dir = tempfile::Builder::new()
        .prefix("nautilus-vm-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let result = run(command, dir.path(), Duration::from_secs(20)).await;
    dir.close()?;
    result
}

async fn run(mut command: Command, dir: &Path, budget: Duration) -> io::Result<()> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    command
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    {
        let parent = std::process::id();
        // Kill this VMM even if the CLI itself dies before it can reap the child.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() as u32 != parent {
                    return Err(io::Error::from_raw_os_error(libc::ECHILD));
                }
                Ok(())
            });
        }
    }
    let mut child = command.spawn()?;
    let socket = dir.join("vsock.sock");
    let result = tokio::select! {
        status = child.wait() => return Err(io::Error::other(format!("VMM exited before replying: {}", status?))),
        reply = timeout(budget, wait_for_guest(&socket)) => reply.map_err(|_| {
            io::Error::new(ErrorKind::TimedOut, "timed out waiting for the guest vsock probe")
        }).and_then(|r| r),
        _ = interrupt.recv() => Err(io::Error::new(ErrorKind::Interrupted, "VM probe interrupted")),
        _ = terminate.recv() => Err(io::Error::new(ErrorKind::Interrupted, "VM probe terminated")),
    };
    if let Err(cleanup) = terminate_vmm(&mut child).await {
        return Err(match result {
            Err(probe) => io::Error::new(cleanup.kind(), format!("{probe}; {cleanup}")),
            Ok(()) => cleanup,
        });
    }
    result
}

async fn terminate_vmm(child: &mut Child) -> io::Result<()> {
    let Some(pid) = child.id() else {
        return Ok(());
    };
    if let Err(e) = child.start_kill() {
        // The guest can exit between the handshake and the signal.
        if e.raw_os_error() != Some(libc::ESRCH) && !matches!(child.try_wait(), Ok(Some(_))) {
            return Err(io::Error::new(
                e.kind(),
                format!("SIGKILL failed for VMM PID {pid}, exit unconfirmed: {e}"),
            ));
        }
    }
    wait_for_exit(child, pid, Duration::from_secs(2)).await
}

async fn wait_for_exit(child: &mut Child, pid: u32, budget: Duration) -> io::Result<()> {
    match timeout(budget, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(io::Error::new(
            e.kind(),
            format!("VMM PID {pid} exit unconfirmed: {e}"),
        )),
        Err(_) => Err(io::Error::new(
            ErrorKind::TimedOut,
            format!("VMM PID {pid} exit unconfirmed {budget:?} after SIGKILL"),
        )),
    }
}

async fn wait_for_guest(socket: &Path) -> io::Result<()> {
    let mut stream = loop {
        match connect_guest(socket, 4081).await {
            Ok(stream) => break stream,
            // The VMM can listen before the guest has bound its port. In that
            // window it may close the stream without acknowledging CONNECT.
            Err(e)
                if !matches!(
                    e.kind(),
                    ErrorKind::NotFound
                        | ErrorKind::ConnectionRefused
                        | ErrorKind::UnexpectedEof
                        | ErrorKind::ConnectionReset
                        | ErrorKind::BrokenPipe
                ) =>
            {
                return Err(e)
            }
            _ => sleep(Duration::from_millis(50)).await,
        }
    };
    // An acknowledged connection has reached the guest. Send one request and
    // give that peer the remaining probe budget to reply.
    stream.get_mut().write_all(b"PING 1\n").await?;
    let mut response = Vec::new();
    stream.take(7).read_until(b'\n', &mut response).await?;
    if response != b"PONG 1\n" {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "invalid guest response; expected PONG 1",
        ));
    }
    Ok(())
}

pub(crate) async fn connect_guest(socket: &Path, port: u32) -> io::Result<BufReader<UnixStream>> {
    let mut stream = BufReader::new(UnixStream::connect(socket).await?);
    stream
        .get_mut()
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .await?;
    // Cloud Hypervisor replies with its assigned host-side port.
    let mut header = String::new();
    if (&mut stream).take(16).read_line(&mut header).await? == 0 {
        return Err(ErrorKind::UnexpectedEof.into());
    }
    let port = header
        .strip_prefix("OK ")
        .and_then(|v| v.strip_suffix('\n'))
        .filter(|v| v.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|v| v.parse::<u32>().ok());
    if port.is_none() {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "invalid Cloud Hypervisor vsock acknowledgement",
        ));
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    // Stand in for the guest and keep a real VMM child to check reaping.
    async fn exercise(reply: Option<&[u8]>, budget: Duration) -> io::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("vsock.sock")).unwrap();
        let pid_file = dir.path().join("pid");
        let mut command = Command::new("sh");
        command.args(["-c", "echo $$ > pid; exec sleep 60"]);
        let guest = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut peer = BufReader::new(stream);
            let mut line = String::new();
            peer.read_line(&mut line).await.unwrap();
            assert_eq!(line, "CONNECT 4081\n");
            peer.get_mut().write_all(b"OK 1073741824\n").await.unwrap();
            line.clear();
            peer.read_line(&mut line).await.unwrap();
            assert_eq!(line, "PING 1\n");
            while !pid_file.exists() {
                sleep(Duration::from_millis(5)).await;
            }
            if let Some(reply) = reply {
                peer.get_mut().write_all(reply).await.unwrap();
            } else {
                // Leave the connection open without a response until the deadline.
                peer.read_line(&mut line).await.unwrap();
            }
        };
        let (result, guest) = tokio::join!(
            run(command, dir.path(), budget),
            timeout(Duration::from_secs(5), guest)
        );
        guest.expect("the guest should receive the request and disconnect");
        let pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        result
    }

    #[tokio::test]
    async fn verifies_guest_and_reaps_vmm() {
        exercise(Some(b"PONG 1\n"), Duration::from_secs(2))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rejects_incorrect_pong_and_reaps_vmm() {
        let err = exercise(Some(b"ERR\n"), Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    async fn delayed_exchange_uses_one_connection(delay_ack: bool) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vsock.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let guest = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut peer = BufReader::new(stream);
            let mut request = String::new();
            for (expected, reply, delayed) in [
                ("CONNECT 4081\n", "OK 1073741824\n", delay_ack),
                ("PING 1\n", "PONG 1\n", !delay_ack),
            ] {
                request.clear();
                peer.read_line(&mut request).await.unwrap();
                assert_eq!(request, expected);
                if delayed {
                    tokio::select! {
                        _ = sleep(Duration::from_millis(1200)) => (),
                        _ = listener.accept() => panic!("client opened a second connection"),
                        _ = peer.read_line(&mut request) => panic!("client closed or reused the stream before {reply:?}"),
                    }
                }
                peer.get_mut().write_all(reply.as_bytes()).await.unwrap();
            }
        };
        timeout(Duration::from_secs(4), async {
            let (result, ()) = tokio::join!(wait_for_guest(&socket), guest);
            result.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn slow_pong_uses_the_acknowledged_connection_once() {
        delayed_exchange_uses_one_connection(false).await;
    }

    #[tokio::test]
    async fn slow_ack_uses_the_original_connection() {
        delayed_exchange_uses_one_connection(true).await;
    }

    #[tokio::test]
    async fn unresponsive_guest_times_out_and_reaps_vmm() {
        let err = exercise(None, Duration::from_millis(300))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn reap_timeout_identifies_child_and_repeated_termination_succeeds() {
        // A live child exercises the wait deadline without forcing kernel I/O to stall.
        let mut child = Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let err = timeout(
            Duration::from_secs(1),
            wait_for_exit(&mut child, pid, Duration::from_millis(25)),
        )
        .await
        .expect("reap must respect its deadline")
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert!(err
            .to_string()
            .contains(&format!("VMM PID {pid} exit unconfirmed")));
        terminate_vmm(&mut child).await.unwrap();
        terminate_vmm(&mut child).await.unwrap();
    }

    #[tokio::test]
    async fn early_vmm_exit_reports_status_and_removes_private_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let executable = fixture.path().join("vmm");
        std::fs::write(
            &executable,
            "#!/bin/sh\nout=$(dirname \"$0\")\npwd > \"$out/cwd\"\nls -ld . > \"$out/mode\"\nexit 17\n",
        ).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let err = probe(&executable, &executable, &executable)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("17"), "{err}");
        let cwd = std::fs::read_to_string(fixture.path().join("cwd")).unwrap();
        assert!(!Path::new(cwd.trim()).exists());
        let mode = std::fs::read_to_string(fixture.path().join("mode")).unwrap();
        assert!(mode.starts_with("drwx------"), "{mode}");
    }
}
