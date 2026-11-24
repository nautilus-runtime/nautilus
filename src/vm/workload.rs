//! Framed, launch-bound workload protocol served by the Nautilus guest agent.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{self, ErrorKind};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use uuid::Uuid;

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Identity {
    version: u32,
    sandbox_id: String,
    nonce: String,
    boot_id: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub(crate) struct StreamOptions {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub tty: bool,
}

#[derive(Serialize)]
pub(crate) struct PortOptions {
    pub port: u16,
}

#[derive(Serialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub(crate) enum Operation<'a> {
    ExecStream {
        argv: &'a [String],
        stream: StreamOptions,
    },
    Attach {
        stream: StreamOptions,
    },
    PortForward {
        stream: PortOptions,
    },
    Create,
    Start,
    Status,
    Exec {
        argv: &'a [String],
        #[serde(rename = "timeoutMs")]
        timeout_ms: i64,
    },
    Stop {
        #[serde(rename = "timeoutMs")]
        timeout_ms: i64,
    },
    Remove,
    ReadLog {
        offset: i64,
        limit: u32,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Request<'a> {
    #[serde(flatten)]
    target: &'a Identity,
    request_id: String,
    container_id: &'a str,
    #[serde(flatten)]
    operation: Operation<'a>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Reply {
    version: u32,
    request_id: String,
    ok: bool,
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
    pub state: Option<String>,
    pub created_at: Option<i64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i32>,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    pub next_offset: Option<i64>,
}

impl Reply {
    pub fn stdout(&self) -> io::Result<Vec<u8>> {
        STANDARD.decode(&self.stdout).map_err(invalid)
    }
    pub fn stderr(&self) -> io::Result<Vec<u8>> {
        STANDARD.decode(&self.stderr).map_err(invalid)
    }
}

fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, e.to_string())
}

async fn read_frame<T: DeserializeOwned>(
    stream: &mut BufReader<UnixStream>,
    limit: u32,
) -> io::Result<T> {
    let len = stream.read_u32().await?;
    if len == 0 || len > limit {
        return Err(invalid("invalid guest frame length"));
    }
    let mut data = vec![0; len as usize];
    stream.read_exact(&mut data).await?;
    serde_json::from_slice(&data).map_err(invalid)
}

pub(crate) struct Channel {
    stream: BufReader<UnixStream>,
    target: Identity,
}

impl Channel {
    pub async fn authenticate(
        mut stream: BufReader<UnixStream>,
        sandbox: &str,
        nonce: &str,
        boot: &str,
    ) -> io::Result<Self> {
        let target: Identity = read_frame(&mut stream, 1024).await?;
        if target.version != 1
            || target.sandbox_id != sandbox
            || target.nonce != nonce
            || target.boot_id != boot
        {
            return Err(invalid("guest workload startup identity mismatch"));
        }
        Ok(Self { stream, target })
    }

    pub async fn call(mut self, id: &str, operation: Operation<'_>) -> io::Result<Reply> {
        self.exchange(id, operation).await
    }

    pub(super) async fn open_stream(
        mut self,
        id: &str,
        operation: Operation<'_>,
    ) -> io::Result<BufReader<UnixStream>> {
        self.exchange(id, operation).await?;
        Ok(self.stream)
    }

    async fn exchange(&mut self, id: &str, operation: Operation<'_>) -> io::Result<Reply> {
        if !super::guest::canonical_uuid(id) {
            return Err(invalid("invalid container ID"));
        }
        let wait = match &operation {
            Operation::Exec { timeout_ms: 0, .. } => None,
            Operation::Exec { timeout_ms, .. } | Operation::Stop { timeout_ms } => Some(
                Duration::from_millis((*timeout_ms).try_into().map_err(invalid)?)
                    .saturating_add(Duration::from_secs(10)),
            ),
            _ => Some(Duration::from_secs(30)),
        };
        let request = Request {
            target: &self.target,
            request_id: Uuid::new_v4().to_string(),
            container_id: id,
            operation,
        };
        let body = serde_json::to_vec(&request).map_err(invalid)?;
        if body.len() > 65536 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "workload request exceeds 64 KiB",
            ));
        }
        let exchange = async {
            self.stream.get_mut().write_u32(body.len() as u32).await?;
            self.stream.get_mut().write_all(&body).await?;
            let result: Reply = read_frame(&mut self.stream, 1 << 20).await?;
            if result.version != 1 || result.request_id != request.request_id {
                return Err(invalid("guest response does not match the request"));
            }
            if !result.ok {
                let kind = match result.code.as_str() {
                    "not_found" => ErrorKind::NotFound,
                    "timeout" => ErrorKind::TimedOut,
                    "invalid" => ErrorKind::InvalidInput,
                    "identity" => ErrorKind::PermissionDenied,
                    "conflict" => ErrorKind::AlreadyExists,
                    "limit" => ErrorKind::InvalidData,
                    _ => ErrorKind::Other,
                };
                return Err(io::Error::new(kind, result.message));
            }
            Ok(result)
        };
        if let Some(wait) = wait {
            tokio::time::timeout(wait, exchange).await.map_err(|_| {
                io::Error::new(ErrorKind::TimedOut, "guest workload operation timed out")
            })?
        } else {
            exchange.await
        }
    }
}
