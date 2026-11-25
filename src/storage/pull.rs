//! Native OCI registry client.
//!
//! Pulls images over the OCI distribution protocol (rustls, no docker CLI)
//! into a content-addressed layer cache under `<image_dir>/layers/`, verifies
//! digests while streaming, and materializes rootfs trees with whiteout rules.
//! This is the foundation of the single-binary edge story: no container CLI
//! anywhere on the host.

use crate::core::{Config, Error, Result};
use oci_distribution::client::{current_platform_resolver, ClientConfig, ClientProtocol};
use oci_distribution::manifest::{
    OciImageManifest, OciManifest, IMAGE_MANIFEST_LIST_MEDIA_TYPE, IMAGE_MANIFEST_MEDIA_TYPE,
    OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE,
};
use oci_distribution::{secrets::RegistryAuth, Client, Reference};
use sha2::{Digest, Sha256, Sha512};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

/// Workload defaults extracted from the image config (typed).
pub struct ImageWorkload {
    pub image_id: String,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub working_dir: String,
    pub user: String,
}

/// A fully cached image: blobs on disk, manifest resolved for this platform.
struct CachedImage {
    manifest: OciImageManifest,
    config: oci_distribution::config::ConfigFile,
    image_id: String,
    /// Digest of the resolved platform manifest (`sha256:...`), the digest
    /// half of a `repo@digest` reference. Empty for cache files written
    /// before this field existed.
    manifest_digest: String,
    blobs: Vec<(PathBuf, String)>, // (path, media_type) in layer order
}

fn client() -> Client {
    Client::new(ClientConfig {
        protocol: ClientProtocol::Https,
        ..Default::default()
    })
}

fn parse_ref(image_ref: &str) -> Result<Reference> {
    image_ref
        .parse::<Reference>()
        .map_err(|e| Error::Image(format!("invalid image reference {image_ref:?}: {e}")))
}

/// Rewrite a docker.io image reference onto a registry mirror (e.g. for
/// deployments where the upstream registry is unreachable). Non-default
/// registries pass through untouched.
fn apply_mirror(image_ref: &str, mirror: &str) -> String {
    let mut s = image_ref.trim();
    for prefix in ["https://", "http://"] {
        s = s.strip_prefix(prefix).unwrap_or(s);
    }
    for prefix in ["docker.io/", "index.docker.io/", "registry-1.docker.io/"] {
        s = s.strip_prefix(prefix).unwrap_or(s);
    }
    // Official images live under the implicit "library/" namespace.
    let repo = if !s.contains('/') {
        format!("library/{s}")
    } else {
        s.to_string()
    };
    format!("{mirror}/{repo}")
}

/// Resolve an image reference, honoring the configured registry mirror.
pub(super) fn resolve_ref(config: &Config, image_ref: &str) -> Result<Reference> {
    let mirror = config.storage.registry_mirror.trim();
    if mirror.is_empty() {
        return parse_ref(image_ref);
    }
    let mirrored = apply_mirror(image_ref, mirror);
    tracing::debug!(from = %image_ref, to = %mirrored, "using registry mirror");
    parse_ref(&mirrored)
}

fn layer_cache_dir(config: &Config) -> PathBuf {
    let dir = config.storage.image_dir.join("layers");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn blob_path(cache_dir: &Path, digest: &str) -> PathBuf {
    cache_dir.join(format!("{}.blob", digest.replace(':', "-")))
}

/// Cache file for a resolved manifest + image config, keyed by reference.
pub(super) fn manifest_cache_path(config: &Config, reference: &Reference) -> PathBuf {
    let dir = config.storage.image_dir.join("manifests");
    let _ = std::fs::create_dir_all(&dir);
    let safe: String = reference
        .whole()
        .chars()
        .map(|c| if c.is_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .collect();
    dir.join(format!("{safe}.json"))
}

/// Try to serve the image fully offline: cached manifest+config AND every
/// layer blob present. Returns None when anything is missing.
async fn load_cached(config: &Config, reference: &Reference) -> Option<CachedImage> {
    let text = tokio::fs::read_to_string(manifest_cache_path(config, reference))
        .await
        .ok()?;
    let file: CachedManifestFile = serde_json::from_str(&text).ok()?;
    let cache_dir = layer_cache_dir(config);
    let mut blobs = Vec::with_capacity(file.manifest.layers.len());
    for desc in &file.manifest.layers {
        let path = blob_path(&cache_dir, &desc.digest);
        if !path.exists() {
            return None; // incomplete — fall through to a network fetch
        }
        blobs.push((path, desc.media_type.clone()));
    }
    let image_id = if file.image_id.is_empty() {
        file.manifest.config.digest.clone()
    } else {
        file.image_id
    };
    Some(CachedImage {
        manifest: file.manifest,
        config: file.config,
        image_id,
        manifest_digest: file.manifest_digest.clone(),
        blobs,
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedManifestFile {
    manifest: OciImageManifest,
    config: oci_distribution::config::ConfigFile,
    #[serde(default)]
    image_id: String,
    #[serde(default)]
    manifest_digest: String,
}

/// AsyncWriter that hashes what it forwards, for on-the-fly digest checks.
struct HashingWriter<'a, W: AsyncWrite + Unpin> {
    inner: &'a mut W,
    hasher: Sha256,
}

impl<'a, W: AsyncWrite + Unpin> AsyncWrite for HashingWriter<'a, W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::task::Poll;
        match std::pin::Pin::new(&mut *self.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                self.hasher.update(&buf[..n]);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.inner).poll_shutdown(cx)
    }
}

const MANIFEST_MEDIA_TYPES: &[&str] = &[
    IMAGE_MANIFEST_MEDIA_TYPE,
    IMAGE_MANIFEST_LIST_MEDIA_TYPE,
    OCI_IMAGE_MEDIA_TYPE,
    OCI_IMAGE_INDEX_MEDIA_TYPE,
];

fn verify_digest(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = if expected.starts_with("sha256:") {
        format!("sha256:{:x}", Sha256::digest(bytes))
    } else if expected.starts_with("sha512:") {
        format!("sha512:{:x}", Sha512::digest(bytes))
    } else {
        return Err(Error::Image(format!("unsupported OCI digest {expected:?}")));
    };
    if actual != expected {
        return Err(Error::Image(format!(
            "OCI content digest mismatch: expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

fn verify_size(bytes: &[u8], expected: i64) -> Result<()> {
    if usize::try_from(expected) != Ok(bytes.len()) {
        return Err(Error::Image(format!(
            "OCI descriptor size mismatch: expected {expected}, got {}", bytes.len()
        )));
    }
    Ok(())
}

async fn pull_manifest(
    client: &Client,
    reference: &Reference,
    auth: &RegistryAuth,
    size: Option<i64>,
) -> Result<(OciManifest, String)> {
    let (body, reported_digest) = client
        .pull_manifest_raw(reference, auth, MANIFEST_MEDIA_TYPES)
        .await
        .map_err(|e| Error::Image(format!("pull manifest {reference}: {e}")))?;
    if let Some(size) = size {
        verify_size(&body, size)?;
    }
    verify_digest(&body, &reported_digest)?;
    if let Some(expected) = reference.digest().filter(|expected| *expected != reported_digest) {
        verify_digest(&body, expected)?;
    }
    let manifest = serde_json::from_slice(&body)
        .map_err(|e| Error::Image(format!("parse manifest {reference}: {e}")))?;
    let (schema, media_type) = match &manifest {
        OciManifest::Image(image) => (image.schema_version, image.media_type.as_deref()),
        OciManifest::ImageIndex(index) => (index.schema_version, index.media_type.as_deref()),
    };
    if schema != 2 || media_type.is_some_and(|media| !MANIFEST_MEDIA_TYPES.contains(&media)) {
        return Err(Error::Image(format!("unsupported OCI manifest format for {reference}")));
    }
    Ok((manifest, reference.digest().map_or(reported_digest, str::to_owned)))
}

async fn pull_image_manifest(
    client: &Client,
    reference: &Reference,
    auth: &RegistryAuth,
) -> Result<(OciImageManifest, String)> {
    let (manifest, digest) = pull_manifest(client, reference, auth, None).await?;
    match manifest {
        OciManifest::Image(image) => Ok((image, digest)),
        OciManifest::ImageIndex(index) => {
            let entry = current_platform_resolver(&index.manifests)
                .and_then(|digest| index.manifests.iter().find(|entry| entry.digest == digest))
                .ok_or_else(|| Error::Image(format!("no image in {reference} matches this node")))?;
            let pinned = Reference::with_digest(
                reference.registry().into(), reference.repository().into(), entry.digest.clone(),
            );
            match pull_manifest(client, &pinned, auth, Some(entry.size)).await? {
                (OciManifest::Image(image), digest) => Ok((image, digest)),
                _ => Err(Error::Image(format!("expected an image manifest at {pinned}"))),
            }
        }
    }
}

impl CachedImage {
    /// Resolve the platform manifest and make sure every layer blob is in the
    /// content-addressed cache (streaming download + sha256 verify).
    async fn fetch(
        config: &Config,
        client: &Client,
        reference: &Reference,
        auth: &RegistryAuth,
        publish_ref: bool,
    ) -> Result<Self> {
        let (manifest, manifest_digest) = pull_image_manifest(client, reference, auth).await?;

        // Fetch and parse the image configuration object.
        let cfg_desc = &manifest.config;
        let mut cfg_bytes = Vec::new();
        client
            .pull_blob(reference, cfg_desc, &mut cfg_bytes)
            .await
            .map_err(|e| Error::Image(format!("pull image config: {e}")))?;
        verify_size(&cfg_bytes, cfg_desc.size)?;
        verify_digest(&cfg_bytes, &cfg_desc.digest)?;
        // OCI ImageID uses SHA-256 even when the configuration descriptor
        // uses another supported digest algorithm.
        let image_id = format!("sha256:{:x}", Sha256::digest(&cfg_bytes));
        let config_file: oci_distribution::config::ConfigFile = serde_json::from_slice(&cfg_bytes)
            .map_err(|e| Error::Image(format!("parse image config: {e}")))?;

        // Stream each layer into the cache, verifying its digest.
        let cache_dir = layer_cache_dir(config);
        let mut blobs = Vec::with_capacity(manifest.layers.len());
        for desc in &manifest.layers {
            let path = blob_path(&cache_dir, &desc.digest);
            if !path.exists() {
                let (file, tmp) = tempfile::NamedTempFile::new_in(&cache_dir)?.into_parts();
                let mut file = tokio::fs::File::from_std(file);
                let mut hw = HashingWriter {
                    inner: &mut file,
                    hasher: Sha256::new(),
                };
                client
                    .pull_blob(reference, desc, &mut hw)
                    .await
                    .map_err(|e| Error::Image(format!("pull layer {}: {e}", desc.digest)))?;
                hw.inner
                    .flush()
                    .await
                    .map_err(|e| Error::Image(format!("flush layer blob: {e}")))?;
                hw.inner
                    .shutdown()
                    .await
                    .map_err(|e| Error::Image(format!("close layer blob: {e}")))?;

                let got = format!("sha256:{:x}", hw.hasher.finalize());
                if got != desc.digest {
                    return Err(Error::Image(format!(
                        "layer digest mismatch: want {}, got {got}",
                        desc.digest
                    )));
                }
                tmp.persist(&path).map_err(|e| e.error)?;
            }
            blobs.push((path, desc.media_type.clone()));
        }

        // Persist manifest+config so future materializations are offline.
        let cache_file = CachedManifestFile {
            manifest: manifest.clone(),
            config: config_file.clone(),
            image_id: image_id.clone(),
            manifest_digest: manifest_digest.clone(),
        };
        let json = serde_json::to_vec(&cache_file)?;
        let immutable = Reference::with_digest(reference.registry().to_owned(), reference.repository().to_owned(), manifest_digest.clone());
        super::atomic_write(&manifest_cache_path(config, &immutable), &json).await?;
        if publish_ref && reference != &immutable {
            super::atomic_write(&manifest_cache_path(config, reference), &json).await?;
        }

        Ok(Self {
            manifest,
            config: config_file,
            image_id,
            manifest_digest,
            blobs,
        })
    }

    fn workload(&self) -> ImageWorkload {
        let cfg = self.config.config.as_ref();
        ImageWorkload {
            image_id: self.image_id.clone(),
            entrypoint: cfg
                .and_then(|c| c.entrypoint.clone())
                .unwrap_or_default(),
            cmd: cfg.and_then(|c| c.cmd.clone()).unwrap_or_default(),
            env: cfg.and_then(|c| c.env.clone()).unwrap_or_default(),
            user: cfg.and_then(|c| c.user.clone()).unwrap_or_default(),
            working_dir: cfg
                .and_then(|c| c.working_dir.clone())
                .unwrap_or_else(|| "/".into()),
        }
    }
}

/// Make sure all layers of `image_ref` are present in the local cache.
/// Callers that only need compressed layers can skip preparing a runtime snapshot.
pub async fn prefetch_layers(config: &Config, image_ref: &str, auth: RegistryAuth) -> Result<String> {
    Ok(fetch_for_pull(config, image_ref, auth).await?.0)
}

async fn fetch_for_pull(config: &Config, image_ref: &str, auth: RegistryAuth) -> Result<(String, CachedImage)> {
    let reference = resolve_ref(config, image_ref)?;
    if let Some(cached) = cached_for_pull(config, &reference).await? {
        return Ok((reference.whole(), cached));
    }
    let cached = CachedImage::fetch(config, &client(), &reference, &auth, false).await?;
    let pinned = Reference::with_digest(
        reference.registry().to_owned(), reference.repository().to_owned(), cached.manifest_digest.clone(),
    );
    Ok((pinned.whole(), cached))
}

pub(super) async fn stage_pull(
    config: &Config, image_ref: &str, auth: RegistryAuth,
) -> Result<(String, Option<super::snapshot::StagedTree>)> {
    let (pinned, cached) = fetch_for_pull(config, image_ref, auth).await?;
    if !config.storage.overlay.enabled {
        return Ok((pinned, None));
    }
    let dir = config.storage.container_dir.clone();
    let options = config.storage.overlay.mount_options.clone();
    let image_dir = config.storage.image_dir.clone();
    let staged = tokio::task::spawn_blocking(move || {
        super::snapshot::stage_pull(&dir, &cached.snapshot_key()?, &options, |dst| cached.unpack(dst, &image_dir))
    }).await.map_err(|e| Error::Storage(format!("stage rootfs snapshot: {e}")))??;
    Ok((pinned, Some(staged)))
}

/// After the index intent is durable, publish the downloaded immutable
/// manifest under its requested reference. This operation can be replayed.
pub(super) async fn publish_cached(
    config: &Config, image_ref: &str, requested: &str, pinned: &str,
    prepare_snapshot: bool, staged: Option<super::snapshot::StagedTree>, update: super::image::IndexGuard,
) -> Result<ImageFacts> {
    let pinned = parse_ref(pinned)?;
    let requested = parse_ref(requested)?;
    let cached = load_cached(config, &pinned).await.ok_or_else(|| Error::Image(format!("missing cached manifest for {pinned}")))?;
    let facts = cached.facts(image_ref);
    let dir = config.storage.container_dir.clone();
    let overlay = config.storage.overlay.clone();
    let image_dir = config.storage.image_dir.clone();
    let facts = tokio::task::spawn_blocking(move || -> Result<ImageFacts> {
        let _guard = update;
        if prepare_snapshot && overlay.enabled {
            super::snapshot::publish_pull(
                &dir, &cached.snapshot_key()?, &overlay.mount_options, staged,
                |dst| cached.unpack(dst, &image_dir),
            )?;
        } else {
            super::snapshot::retain(&dir, &facts.config_digest, &facts.manifest_digest)?;
        }
        Ok(facts)
    }).await.map_err(|e| Error::Storage(format!("publish rootfs snapshot: {e}")))??;
    if requested != pinned {
        let data = tokio::fs::read(manifest_cache_path(config, &pinned)).await?;
        super::atomic_write(&manifest_cache_path(config, &requested), &data).await?;
    }
    Ok(facts)
}

async fn cached_for_pull(config: &Config, reference: &Reference) -> Result<Option<CachedImage>> {
    if reference.digest().is_some() {
        return Ok(load_cached(config, reference).await);
    }
    pin_cached_tag(config, reference).await?;
    // A mutable tag must always be looked up in the registry on explicit pull.
    Ok(None)
}

/// Capture both digests and publish the immutable alias from one read of
/// the tag. Concurrent refreshes can replace that tag after we open it.
async fn pin_cached_tag(config: &Config, reference: &Reference) -> Result<Option<(String, String)>> {
    let json = match tokio::fs::read(manifest_cache_path(config, reference)).await {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Ok(old) = serde_json::from_slice::<CachedManifestFile>(&json) else { return Ok(None) };
    let digest = reference.digest().unwrap_or(&old.manifest_digest);
    if digest.is_empty() || (!old.manifest_digest.is_empty() && old.manifest_digest != digest) {
        return Ok(None);
    }
    let pinned = Reference::with_digest(reference.registry().to_owned(), reference.repository().to_owned(), digest.to_owned());
    let alias = manifest_cache_path(config, &pinned);
    if reference != &pinned && !tokio::fs::try_exists(&alias).await? {
        super::atomic_write(&alias, &json).await?;
    }
    let image_id = if old.image_id.is_empty() {
        old.manifest.config.digest
    } else {
        old.image_id
    };
    Ok(Some((image_id, digest.to_owned())))
}

pub(super) async fn cached_manifest_for_id(config: &Config, image_ref: &str, id: &str) -> Result<Option<String>> {
    let reference = resolve_ref(config, image_ref)?;
    Ok(pin_cached_tag(config, &reference).await?.and_then(|(config_digest, manifest_digest)| {
        (config_digest == id).then_some(manifest_digest)
    }))
}

/// Registry-level facts derivable from the cached manifest/config without
/// unpacking: the config-blob digest IS docker's IMAGE ID, and `created` is
/// the image build time shown by `docker images`.
pub struct ImageFacts {
    pub config_digest: String,
    /// Digest of the resolved platform manifest — pairs with the repo part
    /// of the reference to form `repo@sha256:...` (CRI RepoDigests). Empty
    /// when unavailable (legacy cache); digest references carry it in the
    /// ref itself.
    pub manifest_digest: String,
    /// Build time from the image config (already parsed by oci-distribution).
    pub created: Option<chrono::DateTime<chrono::Utc>>,
    /// Compressed layer bytes per the manifest — known right after fetch,
    /// before any unpacking (feeds CRI Image.size which must be non-zero).
    pub size: u64,
    /// Image config `User` (e.g. "1002" or "nobody"), for CRI Image.uid/username.
    pub user: String,
}

pub(super) async fn cached_image_facts(config: &Config, image_ref: &str) -> Option<ImageFacts> {
    let reference = resolve_ref(config, image_ref).ok()?;
    Some(load_cached(config, &reference).await?.facts(image_ref))
}

/// Fetch (offline when possible) manifest+config for `image_ref` and extract
/// the docker-parity identity fields. Offline cache hits never need network;
/// misses fetch with the given `auth` (Anonymous for status enrichment, the
/// CRI credential set during PullImage).
pub async fn image_facts(config: &Config, image_ref: &str, auth: RegistryAuth) -> Result<ImageFacts> {
    let reference = resolve_ref(config, image_ref)?;
    let cached = match load_cached(config, &reference).await {
        Some(c) => c,
        None => {
            let c = client();
            CachedImage::fetch(config, &c, &reference, &auth, true).await?
        }
    };
    Ok(cached.facts(image_ref))
}

impl CachedImage {
    fn snapshot_key(&self) -> Result<String> {
        // Old manifest-cache entries may lack a digest. Their resolved OCI
        // manifest still records the ordered layers and configuration.
        let identity = if self.manifest_digest.is_empty() {
            serde_json::to_vec(&self.manifest)?
        } else {
            self.manifest_digest.as_bytes().to_owned()
        };
        Ok(super::snapshot::key(&self.image_id, &identity))
    }

    fn unpack(&self, dst: &Path, image_dir: &Path) -> Result<()> {
        for (path, media_type) in &self.blobs {
            crate::storage::unpack::apply_layer(dst, &std::fs::read(path)?, media_type)?;
        }
        let meta = serde_json::json!({
            "image_id": self.image_id,
            "created": self.config.created.map(|c| c.to_rfc3339()),
            "size": dir_size(dst),
        });
        let sidecar = image_dir.join(format!("{}.nmeta",
            super::image::safe_image_name(&self.image_id)));
        // Size is optional display metadata. Never expose a partial JSON file.
        if let Ok(mut file) = tempfile::NamedTempFile::new_in(image_dir) {
            if serde_json::to_writer(&mut file, &meta).is_ok() {
                let _ = file.persist(sidecar);
            }
        }
        Ok(())
    }

    fn facts(&self, image_ref: &str) -> ImageFacts {
        let size = self.manifest.layers.iter().map(|d| d.size.max(0) as u64).sum::<u64>()
            + self.manifest.config.size.max(0) as u64;
        // A digest reference already names the manifest digest; legacy caches
        // that never stored it can still serve RepoDigests for such refs.
        let manifest_digest = if !self.manifest_digest.is_empty() {
            self.manifest_digest.clone()
        } else {
            image_ref
                .split_once('@')
                .map(|(_, d)| d.to_string())
                .unwrap_or_default()
        };
        ImageFacts {
            config_digest: self.image_id.clone(),
            manifest_digest,
            created: self.config.created,
            size,
            user: self
                .config
                .config
                .as_ref()
                .and_then(|c| c.user.clone())
                .unwrap_or_default(),
        }
    }
}

/// Recursively sum regular-file sizes under `root` (symlinks skipped).
fn dir_size(root: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    if let Ok(md) = entry.metadata() {
                        total += md.len();
                    }
                }
                _ => {}
            }
        }
    }
    total
}

/// Materialize the image rootfs into `rootfs_dir` and return workload defaults.
pub async fn materialize_rootfs(
    config: &Config,
    image_ref: &str,
    rootfs_dir: &Path,
) -> Result<ImageWorkload> {
    prepare_root(config, image_ref, rootfs_dir, false).await
}

pub(crate) async fn prepare_bundle(config: &Config, image_ref: &str, bundle: &Path, overlay: bool) -> Result<ImageWorkload> {
    prepare_root(config, image_ref, &bundle.join("rootfs"), overlay && config.storage.overlay.enabled).await
}

async fn prepare_root(config: &Config, image_ref: &str, rootfs_dir: &Path, overlay: bool) -> Result<ImageWorkload> {
    let reference = resolve_ref(config, image_ref)?;
    // Offline fast path: cached manifest+config and every layer blob present.
    let cached = match load_cached(config, &reference).await {
        Some(c) => c,
        None => {
            // Container creation has no CRI AuthConfig; the image is expected
            // to be pulled (with credentials) beforehand — anonymous covers
            // public registries.
            let c = client();
            CachedImage::fetch(
                config,
                &c,
                &reference,
                &RegistryAuth::Anonymous,
                true,
            )
            .await?
        }
    };

    let workload = cached.workload();
    let root = rootfs_dir.to_owned();
    let image_dir = config.storage.image_dir.clone();
    let options = config.storage.overlay.mount_options.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let key = cached.snapshot_key()?;
        let unpack = |dst: &Path| cached.unpack(dst, &image_dir);
        if overlay {
            super::snapshot::prepare(root.parent().unwrap(), &key, &options, unpack)?;
        } else {
            unpack(&root)?;
        }
        Ok(())
    }).await.map_err(|e| Error::Runtime(format!("rootfs worker: {e}")))??;
    Ok(workload)
}

#[cfg(test)]
mod tests {

    #[test]
    fn registry_metadata_requires_matching_bytes_and_size() {
        let bytes = br#"{"schemaVersion":2}"#;
        let sha256 = format!("sha256:{:x}", Sha256::digest(bytes));
        let sha512 = format!("sha512:{:x}", Sha512::digest(bytes));
        assert!(verify_digest(bytes, &sha256).is_ok());
        assert!(verify_digest(bytes, &sha512).is_ok());
        assert!(verify_digest(b"{}", &sha256).is_err());
        assert!(verify_digest(bytes, "sha1:invalid").is_err());
        assert!(verify_size(bytes, bytes.len() as i64).is_ok());
        assert!(verify_size(bytes, 0).is_err());
        assert!(verify_size(bytes, -1).is_err());
    }

    use super::*;

    #[tokio::test]
    async fn explicit_pull_refreshes_tags_but_preserves_offline_digest_and_create() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.storage.image_dir = dir.path().to_owned();
        let tag = parse_ref("example.test/work:stable").unwrap();
        let manifest_digest = format!("sha256:{}", "a".repeat(64));
        let file = CachedManifestFile {
            manifest: OciImageManifest::default(),
            config: Default::default(),
            image_id: String::new(),
            manifest_digest: manifest_digest.clone(),
        };
        std::fs::write(manifest_cache_path(&cfg, &tag), serde_json::to_vec(&file).unwrap()).unwrap();
        assert!(cached_for_pull(&cfg, &tag).await.unwrap().is_none());
        let pinned = parse_ref(&format!("example.test/work@{manifest_digest}")).unwrap();
        assert_eq!(cached_for_pull(&cfg, &pinned).await.unwrap().unwrap().manifest_digest, manifest_digest);
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        materialize_rootfs(&cfg, &tag.whole(), &rootfs).await.unwrap();
        materialize_rootfs(&cfg, &pinned.whole(), &rootfs).await.unwrap();
    }
}
