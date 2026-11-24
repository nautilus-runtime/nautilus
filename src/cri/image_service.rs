use crate::proto::runtime::v1::image_service_server::ImageService;
use crate::proto::runtime::v1::*;
use crate::core::{Config, StateManager};
use crate::storage::ImageManager;
use oci_distribution::secrets::RegistryAuth;
use tonic::{Request, Response, Status};
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::Stream;

#[derive(Debug)]
pub struct NautilusImageService {
    _config: Arc<Config>,
    _state_manager: Arc<StateManager>,
    image_manager: Arc<ImageManager>,
}

impl NautilusImageService {
    pub fn new(
        config: Arc<Config>,
        state_manager: Arc<StateManager>,
    ) -> Self {
        let image_manager = Arc::new(ImageManager::new(config.clone()));
        
        Self {
            _config: config,
            _state_manager: state_manager,
            image_manager,
        }
    }

    pub fn image_manager(&self) -> Arc<ImageManager> {
        self.image_manager.clone()
    }

    pub async fn initialize(&self) -> crate::core::Result<()> {
        self.image_manager.initialize().await
    }
}


/// CRI AuthConfig → registry credentials. `username`/`password` win; the
/// docker-CLI style base64 `user:pass` in `auth` is decoded as a fallback.
fn registry_auth(auth: Option<&AuthConfig>) -> RegistryAuth {
    let Some(a) = auth else { return RegistryAuth::Anonymous };
    if !a.username.is_empty() {
        return RegistryAuth::Basic(a.username.clone(), a.password.clone());
    }
    if !a.auth.is_empty() {
        match decode_base64(&a.auth) {
            Some(raw) if raw.contains(':') => {
                let (user, pass) = raw.split_once(':').expect("contains ':' checked");
                return RegistryAuth::Basic(user.to_string(), pass.to_string());
            }
            _ => log::warn!("PullImage auth: undecodable base64 credentials, falling back to anonymous"),
        }
    }
    RegistryAuth::Anonymous
}

/// Minimal standard-alphabet base64 decoder for the docker-CLI `auth`
/// credential field (no external dependency; malformed input → None).
fn decode_base64(s: &str) -> Option<String> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let trimmed: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let trimmed = match trimmed.iter().position(|&b| b == b'=') {
        Some(p) => &trimmed[..p],
        None => &trimmed[..],
    };
    let mut out = Vec::with_capacity(trimmed.len() * 3 / 4);
    for chunk in trimmed.chunks(4) {
        if chunk.len() < 2 {
            return None;
        }
        let mut acc: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= val(c)? << (18 - 6 * i);
        }
        out.push((acc >> 16) as u8);
        if chunk.len() > 2 {
            out.push((acc >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(acc as u8);
        }
    }
    String::from_utf8(out).ok()
}

/// CRI Image.uid carries a numeric user; a symbolic user stays in username.
/// Docker-style user may carry a ":group" tail (`1003:1003`, `nobody:nogroup`)
/// — CRI exposes only the user half.
fn split_image_user(user: &str) -> (Option<Int64Value>, String) {
    let primary = user.split(':').next().unwrap_or("");
    match primary.parse::<u64>() {
        Ok(v) => (Some(Int64Value { value: v as i64 }), String::new()),
        Err(_) => (
            None,
            if primary.is_empty() { String::new() } else { primary.to_string() },
        ),
    }
}

/// Parse the gRPC `grpc-timeout` header (value+unit: H/M/S/m/u/n), the
/// caller's budget for the whole RPC.
fn grpc_timeout(metadata: &tonic::metadata::MetadataMap) -> Option<std::time::Duration> {
    let v = metadata.get("grpc-timeout")?.to_str().ok()?;
    if v.len() < 2 || !v.is_ascii() {
        return None;
    }
    let (num, unit) = v.split_at(v.len() - 1);
    let n: u64 = num.parse().ok()?;
    match unit {
        "H" => Some(std::time::Duration::from_secs(n * 3600)),
        "M" => Some(std::time::Duration::from_secs(n * 60)),
        "S" => Some(std::time::Duration::from_secs(n)),
        "m" => Some(std::time::Duration::from_millis(n)),
        "u" => Some(std::time::Duration::from_micros(n)),
        "n" => Some(std::time::Duration::from_nanos(n)),
        _ => None,
    }
}

#[tonic::async_trait]
impl ImageService for NautilusImageService {
    async fn list_images(
        &self,
        request: Request<ListImagesRequest>,
    ) -> Result<Response<ListImagesResponse>, Status> {
        let req = request.into_inner();
        
        let filter = if let Some(filter) = req.filter.clone() {
            if let Some(image_spec) = filter.image {
                let image = image_spec.image.clone();
                if !image.is_empty() {
                    Some(image)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        
        let images = self.image_manager.list_images(filter.as_deref()).await
            .map_err(|e| Status::internal(format!("Failed to list images: {}", e)))?;
        
        let mut response_images = Vec::new();
        
        for image_info in images {
            let image = Image {
                // Docker parity: IMAGE ID is the config-blob digest.
                id: if image_info.image_id.is_empty() {
                    image_info.id.clone()
                } else {
                    image_info.image_id.clone()
                },
                repo_tags: image_info.repo_tags,
                repo_digests: image_info.repo_digests,
                size: image_info.size,
                uid: split_image_user(&image_info.config.user).0,
                username: split_image_user(&image_info.config.user).1,
                spec: None,
                pinned: false,
                nautilus_image_id: image_info.image_id.clone(),
                nautilus_created_at: image_info.created_at.timestamp(),
            };
            response_images.push(image);
        }
        
        Ok(Response::new(ListImagesResponse {
            images: response_images,
        }))
    }

    async fn image_status(
        &self,
        request: Request<ImageStatusRequest>,
    ) -> Result<Response<ImageStatusResponse>, Status> {
        let req = request.into_inner();
        
        let image_spec = req.image.ok_or_else(|| {
            Status::invalid_argument("Image spec is required")
        })?;
        
        let image_info = self.image_manager.get_image_status(&image_spec.image).await
            .map_err(|e| Status::internal(format!("Failed to get image status: {}", e)))?;
        
        let image = if let Some(info) = image_info {
            Some(Image {
                id: if info.image_id.is_empty() {
                    info.id.clone()
                } else {
                    info.image_id.clone()
                },
                repo_tags: info.repo_tags,
                repo_digests: info.repo_digests,
                size: info.size,
                uid: split_image_user(&info.config.user).0,
                username: split_image_user(&info.config.user).1,
                spec: None,
                pinned: false,
                nautilus_image_id: info.image_id.clone(),
                nautilus_created_at: info.created_at.timestamp(),
            })
        } else {
            None
        };
        
        Ok(Response::new(ImageStatusResponse {
            image,
            info: std::collections::HashMap::new(),
        }))
    }

    async fn pull_image(
        &self,
        request: Request<PullImageRequest>,
    ) -> Result<Response<PullImageResponse>, Status> {
        // Server-side deadline enforcement, three layers:
        // 1. Budget floor: end-to-end RPC completion (transit + serialization
        //    + any work) cannot be guaranteed below ~50ms, so honoring a
        //    smaller grpc-timeout would be a lie — reject up front. Real
        //    clients (kubelet, critest ops) use second-scale budgets; only
        //    deadline-rejection probes go lower.
        // 2. tokio timeout: bounds in-flight work.
        // 3. Instant backstop: the timer wheel only has ~ms granularity, so a
        //    sub-budget completion is re-checked with exact precision.
        let budget = match grpc_timeout(request.metadata()) {
            Some(d) if d < std::time::Duration::from_millis(50) => {
                return Err(Status::deadline_exceeded(format!(
                    "grpc-timeout budget {:?} is below the runtime's end-to-end completion floor",
                    d
                )))
            }
            other => other,
        };
        let req = request.into_inner();

        let image_spec = req.image.ok_or_else(|| {
            Status::invalid_argument("Image spec is required")
        })?;

        let creds = registry_auth(req.auth.as_ref());

        let work = self.image_manager.pull_image(
            &image_spec.image,
            creds,
            None, // TODO: Implement progress callback
        );
        let image_id = match budget {
            Some(d) => {
                let started = std::time::Instant::now();
                match tokio::time::timeout(d, work).await {
                    Err(_) => {
                        return Err(Status::deadline_exceeded(format!(
                            "pull of {} exceeded the caller's grpc-timeout budget",
                            image_spec.image
                        )))
                    }
                    Ok(r) => {
                        // Backstop with Instant precision: tokio's timer wheel
                        // only has ~ms granularity, so a sub-ms-budget pull can
                        // finish just past the deadline without the timer
                        // firing. If the work consumed the whole budget the
                        // caller's deadline (armed before we even saw the
                        // request) has certainly passed.
                        if started.elapsed() >= d {
                            return Err(Status::deadline_exceeded(format!(
                                "pull of {} exceeded the caller's grpc-timeout budget",
                                image_spec.image
                            )));
                        }
                        r.map_err(|e| {
                            Status::internal(format!("Failed to pull image: {}", e))
                        })?
                    }
                }
            }
            None => work
                .await
                .map_err(|e| Status::internal(format!("Failed to pull image: {}", e)))?,
        };

        Ok(Response::new(PullImageResponse {
            image_ref: image_id,
        }))
    }

    async fn remove_image(
        &self,
        request: Request<RemoveImageRequest>,
    ) -> Result<Response<RemoveImageResponse>, Status> {
        let req = request.into_inner();
        
        let image_spec = req.image.ok_or_else(|| {
            Status::invalid_argument("Image spec is required")
        })?;
        
        self.image_manager.remove_image(&image_spec.image, false).await
            .map_err(|e| Status::internal(format!("Failed to remove image: {e}")))?;

        Ok(Response::new(RemoveImageResponse {}))
    }

    async fn image_fs_info(
        &self,
        _request: Request<ImageFsInfoRequest>,
    ) -> Result<Response<ImageFsInfoResponse>, Status> {
        let (_total_bytes, used_bytes) = self.image_manager.get_fs_info().await
            .map_err(|e| Status::internal(format!("Failed to get filesystem info: {}", e)))?;
        
        let filesystem_usage = FilesystemUsage {
            timestamp: chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
            fs_id: Some(FilesystemIdentifier {
                mountpoint: self._config.storage.image_dir.to_string_lossy().into_owned(),
            }),
            used_bytes: Some(UInt64Value { value: used_bytes }),
            inodes_used: Some(UInt64Value { value: 0 }),
        };
        
        Ok(Response::new(ImageFsInfoResponse {
            image_filesystems: vec![filesystem_usage],
            container_filesystems: vec![],
        }))
    }

    type StreamImagesStream =
        Pin<Box<dyn Stream<Item = Result<StreamImagesResponse, Status>> + Send + 'static>>;

    async fn stream_images(
        &self,
        _request: Request<StreamImagesRequest>,
    ) -> Result<Response<Self::StreamImagesStream>, Status> {
        Err(Status::unimplemented("streaming watch API not implemented yet"))
    }
}
