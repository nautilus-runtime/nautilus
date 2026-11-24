//! Identity handshake with the Nautilus guest agent for a Pod VM.

use std::io::{self, ErrorKind};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

pub(super) fn canonical_uuid(value: &str) -> bool {
    Uuid::try_parse(value).is_ok_and(|id| !id.is_nil() && id.hyphenated().to_string() == value)
}

/// Query the guest's launch identity without sending it the expected values.
/// The caller compares the returned boot ID to the one recorded at creation.
pub async fn identify(socket: &Path, sandbox_id: &str, nonce: &str) -> io::Result<String> {
    if !canonical_uuid(sandbox_id) || !canonical_uuid(nonce) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "invalid VM launch identity",
        ));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut stream = super::connect_guest(socket, 4082).await?;
        stream.get_mut().write_all(b"IDENTIFY 1\n").await?;
        let mut response = String::new();
        stream.take(128).read_line(&mut response).await?;
        let prefix = format!("IDENTITY 1 {sandbox_id} {nonce} ");
        let boot = response
            .strip_prefix(&prefix)
            .and_then(|value| value.strip_suffix('\n'))
            .filter(|value| canonical_uuid(value))
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "guest identity mismatch"))?;
        Ok(boot.to_owned())
    })
    .await
    .map_err(|_| io::Error::new(ErrorKind::TimedOut, "guest identity request timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    const POD: &str = "528b0e65-9097-435c-9867-37e6b6ad29bc";
    const NONCE: &str = "53987486-4249-47e4-a749-f6c2a53c2f41";
    const BOOT: &str = "ae5fbecc-02f1-4d55-8675-ac02486f9d0b";

    async fn peer(reply: &str) -> std::io::Result<String> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("vm.sock");
        let listener = UnixListener::bind(&path)?;
        let agent = async {
            let mut stream = BufReader::new(listener.accept().await.unwrap().0);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(line, "CONNECT 4082\n");
            stream.get_mut().write_all(b"OK 10000\n").await.unwrap();
            line.clear();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(line, "IDENTIFY 1\n");
            stream.get_mut().write_all(reply.as_bytes()).await.unwrap();
        };
        let (result, ()) = tokio::join!(identify(&path, POD, NONCE), agent);
        result
    }

    #[tokio::test]
    async fn silent_guest_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vm.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let peer = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut input = String::new();
            stream.read_line(&mut input).await.unwrap();
            stream.get_mut().write_all(b"OK 10000\n").await.unwrap();
            input.clear();
            stream.read_line(&mut input).await.unwrap();
            assert_eq!(input, "IDENTIFY 1\n");
            input.clear();
            assert_eq!(stream.read_line(&mut input).await.unwrap(), 0);
        };
        let check = async { tokio::join!(peer, identify(&path, POD, NONCE)).1 };
        let err = tokio::time::timeout(Duration::from_secs(5), check)
            .await
            .expect("the caller must bound an unanswered query")
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn verifies_guest_startup_identity_before_returning_boot_id() {
        let reply = format!("IDENTITY 1 {POD} {NONCE} {BOOT}\n");
        assert_eq!(peer(&reply).await.unwrap(), BOOT);
        for mismatch in [
            reply.replace(POD, BOOT),
            reply.replace(NONCE, BOOT),
            reply.replace(BOOT, "not-a-uuid"),
            reply.replace("ITY 1", "ITY 2"),
            reply.trim_end().to_owned(),
            "x".repeat(4096),
        ] {
            assert_eq!(
                peer(&mismatch).await.unwrap_err().kind(),
                ErrorKind::InvalidData
            );
        }
    }
}
