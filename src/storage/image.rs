use crate::core::{Config, Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::process::Command;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageInfo {
    pub id: String,
    /// Docker-style IMAGE ID: the sha256 of the image config blob (short form
    /// displayed). Empty for records written before this field existed.
    #[serde(default)]
    pub image_id: String,
    pub repo_tags: Vec<String>,
    pub repo_digests: Vec<String>,
    pub size: u64,
    pub created_at: DateTime<Utc>,
    pub config: ImageConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageConfig {
    pub env: Vec<String>,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub working_dir: String,
    pub user: String,
    pub exposed_ports: HashMap<String, serde_json::Value>,
    pub volumes: HashMap<String, serde_json::Value>,
    pub labels: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImagePullProgress {
    pub status: String,
    pub progress: Option<String>,
    pub progress_detail: Option<ProgressDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressDetail {
    pub current: u64,
    pub total: u64,
}

#[derive(Serialize, Deserialize)]
struct PendingPull {
    image_ref: String,
    requested: String,
    pinned: String,
    // Older journals committed metadata only and may refer to a non-rootfs artifact.
    #[serde(default)]
    prepare_snapshot: bool,
}

// A blocking publisher can outlive its RPC deadline. Keep its index lock until it exits.
pub(super) type IndexGuard = Arc<tokio::sync::OwnedMutexGuard<()>>;

#[derive(Debug)]
pub struct ImageManager {
    config: Arc<Config>,
    image_store_path: PathBuf,
    images: tokio::sync::RwLock<HashMap<String, ImageInfo>>,
    pull_locks: [tokio::sync::Mutex<()>; 32],
    index_update: Arc<tokio::sync::Mutex<()>>,
}


/// Flatten an image ref ('/', ':' etc.) into a filesystem-safe name.
pub(crate) fn safe_image_name(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

impl ImageManager {
    pub fn new(config: Arc<Config>) -> Self {
        let image_store_path = config.storage.image_dir.clone();

        Self {
            config,
            image_store_path,
            images: tokio::sync::RwLock::new(HashMap::new()),
            pull_locks: std::array::from_fn(|_| tokio::sync::Mutex::new(())),
            index_update: Arc::default(),
        }
    }

    /// Initialize the image manager
    pub async fn initialize(&self) -> Result<()> {
        // Create image store directory if it doesn't exist
        fs::create_dir_all(&self.image_store_path).await
            .map_err(|e| Error::Image(format!("Failed to create image store directory: {}", e)))?;
        
        // Load existing images from disk, then collapse legacy/duplicate
        // records that share one config digest (e.g. a pre-native-pull stub
        // `busybox` next to the real `docker.io/library/busybox:latest`).
        self.load_images_from_disk().await?;
        let removed = self.dedup_images_by_digest().await?;
        let update = Arc::new(self.index_update.clone().lock_owned().await);
        self.recover_pull(&update).await?;
        if removed > 0 {
            log::info!("image store dedup removed {} stale duplicate record(s)", removed);
        }
        
        Ok(())
    }

    /// Merge duplicate records that describe the same image digest: the
    /// canonical record (real config + fully-qualified tag) absorbs the
    /// others' tags; stale stubs (empty config, size 0, bare short name)
    /// are deleted from memory and disk.
    async fn dedup_images_by_digest(&self) -> Result<usize> {
        use std::collections::HashMap;

        let snapshot: Vec<ImageInfo> = {
            let images = self.images.read().await;
            images.values().cloned().collect()
        };

        // Group by config digest; records without one can't be proven equal
        // and are left alone.
        let mut groups: HashMap<String, Vec<ImageInfo>> = HashMap::new();
        for info in snapshot {
            if info.image_id.is_empty() {
                continue;
            }
            groups.entry(info.image_id.clone()).or_default().push(info);
        }

        let mut removed = 0usize;
        for (_, mut recs) in groups {
            if recs.len() < 2 {
                continue;
            }
            // Canonical = real layer/config data first, then a
            // fully-qualified repo tag (contains '/'), then newest.
            recs.sort_by(|a, b| {
                let qa = !a.config.env.is_empty() || !a.config.cmd.is_empty();
                let qb = !b.config.env.is_empty() || !b.config.cmd.is_empty();
                qb.cmp(&qa)
                    .then_with(|| {
                        let fa = a.repo_tags.iter().any(|t| t.contains('/'));
                        let fb = b.repo_tags.iter().any(|t| t.contains('/'));
                        fb.cmp(&fa)
                    })
                    .then_with(|| b.created_at.cmp(&a.created_at))
            });
            let (head, rest) = recs.split_first().unwrap();
            let mut canonical = head.clone();
            for dup in rest {
                for t in &dup.repo_tags {
                    if !canonical.repo_tags.iter().any(|old| same_reference(old, t)) {
                        canonical.repo_tags.push(t.clone());
                    }
                }
                for d in &dup.repo_digests {
                    if !canonical.repo_digests.contains(d) {
                        canonical.repo_digests.push(d.clone());
                    }
                }
            }
            self.save_image_info(&canonical).await?;
            for dup in rest {
                let f = self.image_store_path.join(format!("{}.json", safe_image_name(&dup.id)));
                match fs::remove_file(&f).await {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    result => result?,
                }
                self.images.write().await.remove(&dup.id);
                removed += 1;
                log::info!(
                    "dedup: dropped stale image record '{}' (duplicate of {})",
                    dup.id,
                    canonical.id
                );
            }
            self.images.write().await.insert(canonical.id.clone(), canonical);
        }
        Ok(removed)
    }

    /// Pull an image from a registry. Returns the CRI image ID (the config
    /// digest, `sha256:...`) once known, else the reference as pulled.
    /// `auth` carries the CRI request's registry credentials.
    pub async fn pull_image(
        &self,
        image_ref: &str,
        auth: oci_distribution::secrets::RegistryAuth,
        progress_callback: Option<Box<dyn Fn(ImagePullProgress) + Send + Sync>>,
    ) -> Result<String> {
        let _ = progress_callback; // Suppress unused warning until implementation is complete
        log::info!("Pulling image: {}", image_ref);

        // Serialize re-pulls of the same normalized reference through both
        // manifest publication and index update while other pulls can proceed.
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        let requested = super::pull::resolve_ref(&self.config, image_ref)?.whole();
        requested.hash(&mut hash);
        let slot = hash.finish() as usize % self.pull_locks.len();
        let _guard = self.pull_locks[slot].lock().await;
        // A failed extraction must not enter the replay journal or replace a working tag.
        let (pinned, staged) = super::pull::stage_pull(&self.config, image_ref, auth).await?;
        let pending = PendingPull {
            image_ref: image_ref.to_owned(), requested, pinned,
            prepare_snapshot: self.config.storage.overlay.enabled,
        };
        let update = Arc::new(self.index_update.clone().lock_owned().await);
        self.recover_pull(&update).await?;
        super::atomic_write(&self.image_store_path.join("pull.pending"), &serde_json::to_vec(&pending)?).await?;
        self.finish_pull(&pending, staged, &update).await
    }

    async fn recover_pull(&self, update: &IndexGuard) -> Result<()> {
        match fs::read(self.image_store_path.join("pull.pending")).await {
            Ok(data) => {
                self.finish_pull(&serde_json::from_slice(&data)?, None, update).await?;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn finish_pull(&self, pending: &PendingPull, staged: Option<super::snapshot::StagedTree>, update: &IndexGuard) -> Result<String> {
        let facts = super::pull::publish_cached(
            &self.config, &pending.image_ref, &pending.requested, &pending.pinned, pending.prepare_snapshot, staged, update.clone(),
        ).await?;
        let id = self.record_pull(&pending.image_ref, facts).await?;
        fs::remove_file(self.image_store_path.join("pull.pending")).await?;
        Ok(id)
    }

    async fn record_pull(&self, image_ref: &str, facts: super::pull::ImageFacts) -> Result<String> {
        let by_digest = image_ref.contains('@');
        let digest_ref = if by_digest {
            image_ref.to_owned()
        } else {
            format!("{}@{}", repo_part(image_ref), facts.manifest_digest)
        };
        let mut images = self.images.write().await;
        let old_keys: Vec<_> = images.iter().filter(|(_, img)| img.image_id != facts.config_digest &&
            (img.repo_tags.iter().any(|tag| same_reference(tag, image_ref)) || same_reference(&img.id, image_ref)))
            .map(|(key, _)| key.clone()).collect();
        for key in old_keys {
            let mut old = images[&key].clone();
            old.repo_tags.retain(|tag| !same_reference(tag, image_ref));
            // Legacy records used their first tag as the persistent key.
            // Move that key too or ImageStatus will still match the old tag.
            if same_reference(&old.id, image_ref) {
                old.id = old.repo_digests.first().or_else(|| old.repo_tags.first())
                    .cloned().unwrap_or_else(|| old.image_id.clone());
            }
            if !old.id.is_empty() {
                self.save_image_info(&old).await?;
            }
            if key != old.id {
                let path = self.image_store_path.join(format!("{}.json", safe_image_name(&key)));
                match fs::remove_file(path).await {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    result => result?,
                }
            }
            images.remove(&key);
            if !old.id.is_empty() { images.insert(old.id.clone(), old); }
        }
        let mut info = images.values().find(|img| img.image_id == facts.config_digest)
            .cloned().unwrap_or_else(|| ImageInfo {
                id: digest_ref.clone(),
                image_id: facts.config_digest.clone(),
                repo_tags: Vec::new(),
                repo_digests: Vec::new(),
                size: facts.size,
                created_at: facts.created.unwrap_or_else(Utc::now),
                config: ImageConfig {
                    env: Vec::new(), entrypoint: Vec::new(), cmd: Vec::new(),
                    working_dir: "/".to_owned(), user: String::new(),
                    exposed_ports: HashMap::new(), volumes: HashMap::new(), labels: HashMap::new(),
                },
            });
        if !by_digest && !info.repo_tags.iter().any(|tag| same_reference(tag, image_ref)) {
            info.repo_tags.push(image_ref.to_owned());
        }
        if !info.repo_digests.iter().any(|digest| same_reference(digest, &digest_ref)) {
            info.repo_digests.push(digest_ref);
        }
        info.size = info.size.max(facts.size);
        if let Some(created) = facts.created { info.created_at = created; }
        info.config.user = facts.user;
        self.save_image_info(&info).await?;
        images.insert(info.id.clone(), info);
        Ok(facts.config_digest)
    }

    /// List all images
    pub async fn list_images(&self, filter: Option<&str>) -> Result<Vec<ImageInfo>> {
        let images = self.images.read().await;
        let mut result: Vec<ImageInfo> = images.values().cloned().collect();

        // Overlay docker-parity display facts captured at rootfs materialize
        // time (uncompressed size; authoritative config-digest id/build time).
        // Legacy records missing identity get enriched lazily from the content
        // cache (offline) and persisted so the next listing is instant.
        for img in result.iter_mut() {
            if img.image_id.is_empty() {
                if let Ok(facts) = crate::storage::pull::image_facts(
                    self.config.as_ref(),
                    &img.id,
                    oci_distribution::secrets::RegistryAuth::Anonymous,
                )
                .await
                {
                    img.image_id = facts.config_digest.clone();
                    if let Some(created) = facts.created {
                        img.created_at = created;
                    }
                    let mut persisted = img.clone();
                    persisted.image_id = facts.config_digest;
                    self.save_image_info(&persisted).await.ok();
                }
            }
            if let Some(v) = self.sidecar(img).await {
                if let Some(d) = v["image_id"].as_str().filter(|d| !d.is_empty()) {
                    img.image_id = d.to_string();
                }
                if let Some(sz) = v["size"].as_u64() {
                    img.size = sz;
                }
                if let Some(c) = v["created"].as_str() {
                    if let Some(created) = chrono::DateTime::parse_from_rfc3339(c).ok() {
                        img.created_at = created.with_timezone(&Utc);
                    }
                }
            }
        }
        
        if let Some(filter_str) = filter {
            result.retain(|img| {
                img.repo_tags.iter().any(|tag| tag.contains(filter_str)) ||
                img.id.contains(filter_str)
            });
        }
        
        // Sort by created time (newest first)
        result.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        
        Ok(result)
    }

    /// Kubelet uses the config digest returned by PullImage or ImageStatus in
    /// CreateContainer. Select a retained immutable repository manifest for it.
    pub(crate) async fn workload_reference(&self, image: &str) -> Result<String> {
        let Some(digest) = image.strip_prefix("sha256:") else {
            return Ok(image.to_owned());
        };
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(Error::Image(format!("invalid image config digest {image}")));
        }
        let records = self.images.read().await;
        let manifest = records.values().find(|record| record.image_id == image)
            .and_then(|record| record.repo_digests.first())
            .ok_or_else(|| Error::Image(format!("no pulled manifest for image ID {image}")))?;
        Ok(manifest.clone())
    }

    /// Get image status/information
    pub async fn get_image_status(&self, image_ref: &str) -> Result<Option<ImageInfo>> {
        let images = self.images.read().await;
        match locate_image(&images, image_ref) {
            Some(key) => {
                let image = images.get(key).expect("key from locate_image");
                Ok(Some(self.enrich_image(image.clone()).await))
            }
            None => Ok(None),
        }
    }

    /// Backfill lazily-known display fields (size, digest, created) from the
    /// `.nmeta` sidecar and content cache, mirroring list_images.
    async fn enrich_image(&self, mut img: ImageInfo) -> ImageInfo {
        // Always pull facts when identity is missing OR the user field is
        // empty (legacy records predate config-user persistence). This keeps
        // Image.uid/username served even for images pulled before the field
        // existed.
        if img.image_id.is_empty() || img.config.user.is_empty() {
            if let Ok(facts) = crate::storage::pull::image_facts(
                self.config.as_ref(),
                &img.id,
                oci_distribution::secrets::RegistryAuth::Anonymous,
            )
            .await
            {
                img.image_id = facts.config_digest;
                img.size = img.size.max(facts.size);
                if img.config.user.is_empty() {
                    img.config.user = facts.user.clone();
                }
                if let Some(created) = facts.created {
                    img.created_at = created;
                }
            }
        }
        if let Some(v) = self.sidecar(&img).await {
            if let Some(d) = v["image_id"].as_str().filter(|d| !d.is_empty()) {
                img.image_id = d.to_string();
            }
            if let Some(sz) = v["size"].as_u64() {
                img.size = img.size.max(sz);
            }
            if let Some(c) = v["created"].as_str() {
                if let Ok(created) = chrono::DateTime::parse_from_rfc3339(c) {
                    img.created_at = created.with_timezone(&Utc);
                }
            }
        }
        img
    }

    async fn sidecar(&self, image: &ImageInfo) -> Option<serde_json::Value> {
        // New sidecars are shared by immutable image ID. Older stores used
        // the original pull reference, which might now refer to another image.
        for key in [&image.image_id, &image.id] {
            if key.is_empty() { continue; }
            let path = self.image_store_path.join(format!("{}.nmeta", safe_image_name(key)));
            if let Ok(data) = fs::read(path).await {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&data) {
                    if image.image_id.is_empty() || value["image_id"].as_str() == Some(&*image.image_id) {
                        return Some(value);
                    }
                }
            }
        }
        None
    }

    /// Remove an image. Docker semantics for a content-addressed store:
    /// removing one of several tags just untags; removing the last reference
    /// drops the record (blobs stay — they are content-addressed and may be
    /// shared; reclaim them with a future `gc` pass).
    /// `image_ref` may be a full reference, a repo tag, a repo digest, or an
    /// IMAGE-ID (`sha256:` full or hex prefix).
    pub async fn remove_image(&self, image_ref: &str, _force: bool) -> Result<()> {
        log::info!("Removing image: {}", image_ref);
        let update = Arc::new(self.index_update.clone().lock_owned().await);
        self.recover_pull(&update).await?;

        let mut images = self.images.write().await;

        // Locate the record via the same matcher get_image_status uses.
        let target_key = locate_image(&images, image_ref)
            .map(|k| k.to_string())
            .ok_or_else(|| Error::Image(format!("Image not found: {}", image_ref)))?;

        let mut info = images.get(&target_key).cloned().unwrap();

        // Un-tag (or un-digest) when the request named one of several
        // references on this record; the record itself survives until its
        // last reference is removed.
        let asked_by_tag = info.repo_tags.iter().any(|t| same_reference(t, image_ref));
        if asked_by_tag && info.repo_tags.len() > 1 {
            info.repo_tags.retain(|t| !same_reference(t, image_ref));
            self.save_image_info(&info).await?;
            images.insert(info.id.clone(), info);
            log::info!("Untagged {} (record keeps {} tag(s))", image_ref, images.get(&target_key).map(|i| i.repo_tags.len()).unwrap_or(0));
            return Ok(());
        }
        let asked_by_digest = info.repo_digests.iter().any(|d| same_reference(d, image_ref));
        if asked_by_digest && info.repo_digests.len() > 1 {
            info.repo_digests.retain(|d| !same_reference(d, image_ref));
            self.save_image_info(&info).await?;
            images.insert(info.id.clone(), info);
            log::info!("Removed digest ref {} (record keeps {} digest(s))", image_ref, images.get(&target_key).map(|i| i.repo_digests.len()).unwrap_or(0));
            return Ok(());
        }

        // Retire before deleting the index so a failed removal can be retried.
        // Live bundles keep the unpacked tree pinned until their own removal.
        let dir = self.config.storage.container_dir.clone();
        let image_id = info.image_id.clone();
        let retire_guard = update.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = retire_guard;
            super::snapshot::retire(&dir, &image_id)
        })
            .await.map_err(|e| Error::Storage(format!("retire rootfs snapshot: {e}")))??;
        // Full removal of the record.
        let f = self
            .image_store_path
            .join(format!("{}.json", safe_image_name(&target_key)));
        if f.exists() {
            fs::remove_file(f).await
                .map_err(|e| Error::Image(format!("Failed to remove image file: {}", e)))?;
        }
        images.remove(&target_key);
        log::info!("Successfully removed image: {}", target_key);
        Ok(())
    }

    /// Get filesystem information
    pub async fn get_fs_info(&self) -> Result<(u64, u64)> {
        let mut total_size = 0u64;
        let mut used_size = 0u64;
        
        // Get disk usage of image store
        if let Ok(metadata) = fs::metadata(&self.image_store_path).await {
            if metadata.is_dir() {
                used_size = self.calculate_directory_size(&self.image_store_path).await?;
            }
        }
        
        // Get total disk space (simplified - using available space)
        let output = Command::new("df")
            .args(["-B1", self.image_store_path.to_str().unwrap_or("/")])
            .output()
            .await
            .map_err(|e| Error::Runtime(format!("Failed to get disk info: {}", e)))?;
        
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = stdout.lines().nth(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(size) = parts[1].parse::<u64>() {
                        total_size = size;
                    }
                }
            }
        }
        
        Ok((total_size, used_size))
    }


    /// Load images from disk
    async fn load_images_from_disk(&self) -> Result<()> {
        if !self.image_store_path.exists() {
            return Ok(());
        }
        
        let mut dir = fs::read_dir(&self.image_store_path).await
            .map_err(|e| Error::Image(format!("Failed to read image store directory: {}", e)))?;
        
        let mut images = self.images.write().await;
        
        while let Some(entry) = dir.next_entry().await
            .map_err(|e| Error::Image(format!("Failed to read directory entry: {}", e)))? {
            
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path).await {
                    if let Ok(image_info) = serde_json::from_str::<ImageInfo>(&content) {
                        images.insert(image_info.id.clone(), image_info);
                    }
                }
            }
        }
        
        Ok(())
    }

    /// Save image information to disk
    async fn save_image_info(&self, image_info: &ImageInfo) -> Result<()> {
        let image_file = self
            .image_store_path
            .join(format!("{}.json", safe_image_name(&image_info.id)));
        let content = serde_json::to_string_pretty(image_info)
            .map_err(|e| Error::Image(format!("Failed to serialize image info: {}", e)))?;
        
        super::atomic_write(&image_file, content.as_bytes()).await
            .map_err(|e| Error::Image(format!("Failed to write image info: {}", e)))?;
        
        Ok(())
    }

    /// Calculate directory size recursively
    fn calculate_directory_size<'a>(&'a self, dir: &'a Path) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64>> + Send + 'a>> {
        Box::pin(async move {
            let mut total_size = 0u64;
            let mut dir_reader = fs::read_dir(dir).await
                .map_err(|e| Error::Image(format!("Failed to read directory: {}", e)))?;
            
            while let Some(entry) = dir_reader.next_entry().await
                .map_err(|e| Error::Image(format!("Failed to read directory entry: {}", e)))? {
                
                let metadata = entry.metadata().await
                    .map_err(|e| Error::Image(format!("Failed to get metadata: {}", e)))?;
                
                if metadata.is_file() {
                    total_size += metadata.len();
                } else if metadata.is_dir() {
                    total_size += self.calculate_directory_size(&entry.path()).await?;
                }
            }
            
            Ok(total_size)
        })
    }
}

fn same_reference(left: &str, right: &str) -> bool {
    left == right || match (left.parse::<oci_distribution::Reference>(), right.parse::<oci_distribution::Reference>()) {
        (Ok(a), Ok(b)) if a.digest().is_some() && b.digest().is_some() => {
            a.registry() == b.registry() && a.repository() == b.repository() && a.digest() == b.digest()
        }
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Repo part of a reference: everything before `@digest`, and without the
/// `:tag` suffix when one is present (`a/b:c` → `a/b`). A colon inside the
/// path is a registry port (`localhost:5000/x`), not a tag.
fn repo_part(image_ref: &str) -> &str {
    let no_digest = image_ref.split_once('@').map_or(image_ref, |(r, _)| r);
    match no_digest.rsplit_once(':') {
        Some((_, tag)) if !tag.is_empty() && !tag.contains('/') => no_digest
            .strip_suffix(&format!(":{tag}"))
            .unwrap_or(no_digest),
        _ => no_digest,
    }
}

/// Resolve a caller-supplied reference to the store key naming that record:
/// exact key, repo tag, repo digest, or image ID (config digest, with or
/// without the `sha256:` prefix, full or >=4-hex prefix). A bare name (no
/// tag, no digest) also tries the implicit `:latest` form, matching how
/// clients normalize before pulling but not before querying.
fn locate_image<'a>(
    images: &'a HashMap<String, ImageInfo>,
    image_ref: &str,
) -> Option<&'a str> {
    for (key, image) in images {
        // Legacy untagged stubs had only an id. A removed tag or digest
        // must otherwise stay removed, even when it once named the record.
        if (image.repo_tags.is_empty() && image.repo_digests.is_empty() && same_reference(key, image_ref))
            || image.repo_tags.iter().chain(&image.repo_digests).any(|r| same_reference(r, image_ref))
        {
            return Some(key);
        }
    }
    // Image-ID form: `sha256:<hex>` or bare hex prefix (>= 4 chars). Only
    // reached after exact/tag/digest misses, mirroring docker's resolution
    // order (a tag named like a hex string still wins above).
    let hex = image_ref.strip_prefix("sha256:").unwrap_or(image_ref);
    if hex.len() >= 4 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut best: Option<&String> = None;
        for (k, i) in images.iter() {
            if i
                .image_id
                .strip_prefix("sha256:")
                .filter(|id| id.starts_with(hex))
                .is_some()
            {
                best = match best {
                    // Deterministic pick on ambiguous prefixes.
                    Some(cur) if cur.as_str() <= k.as_str() => Some(cur),
                    _ => Some(k),
                };
            }
        }
        if let Some(k) = best {
            return Some(k.as_str());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(id: char) -> super::super::pull::ImageFacts {
        let digest = format!("sha256:{}", id.to_string().repeat(64));
        super::super::pull::ImageFacts {
            config_digest: digest.clone(),
            manifest_digest: digest,
            created: None,
            size: 123,
            user: "10001".into(),
        }
    }

    #[tokio::test]
    async fn moving_tag_keeps_sibling_and_digest_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.image_dir = dir.path().to_owned();
        config.storage.container_dir = dir.path().join("containers");
        let config = Arc::new(config);
        let store = ImageManager::new(config.clone());
        store.initialize().await.unwrap();
        let stable = "example.test/project:stable";
        let previous = "example.test/project:v1";
        let first = store.record_pull(stable, facts('a')).await.unwrap();
        store.record_pull(previous, facts('a')).await.unwrap();
        let second = store.record_pull(stable, facts('b')).await.unwrap();

        let meta = serde_json::json!({"image_id": first, "size": 456});
        let path = dir.path().join(format!("{}.nmeta", safe_image_name(&first)));
        std::fs::write(path, meta.to_string()).unwrap();
        let restarted = ImageManager::new(config);
        restarted.initialize().await.unwrap();
        let latest = restarted.get_image_status(stable).await.unwrap().unwrap();
        let original = restarted.get_image_status(previous).await.unwrap().unwrap();
        assert_eq!(latest.image_id, second);
        assert_eq!(latest.repo_tags, [stable]);
        assert_eq!(original.image_id, first);
        assert_eq!(original.repo_tags, [previous]);
        assert_eq!(original.size, 456);
        assert_eq!(latest.size, 123);
        let old_digest = format!("example.test/project@{first}");
        assert_eq!(restarted.get_image_status(&old_digest).await.unwrap().unwrap().image_id, first);
        assert_eq!(restarted.workload_reference(&first).await.unwrap(), old_digest);
        assert!(restarted.workload_reference(&format!("sha256:{}", "0".repeat(64))).await.is_err());
    }
    #[tokio::test]
    async fn refresh_recovers_pending_manifest_and_normalizes_tag_aliases() {
        use oci_distribution::{config::ConfigFile, manifest::OciImageManifest, Reference};
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.image_dir = dir.path().to_owned();
        config.storage.container_dir = dir.path().join("containers");
        let config = Arc::new(config);
        let store = ImageManager::new(config.clone());
        store.initialize().await.unwrap();
        store.record_pull("busybox:latest", facts('a')).await.unwrap();
        store.record_pull("busybox:v1", facts('a')).await.unwrap();
        let next = facts('b');
        let requested: Reference = "busybox".parse().unwrap();
        let pinned: Reference = format!("busybox@{}", next.manifest_digest).parse().unwrap();
        let mut manifest = OciImageManifest::default();
        manifest.config.digest = next.config_digest.clone();
        let data = serde_json::json!({"manifest":manifest,"config":ConfigFile::default(),"manifest_digest":next.manifest_digest});
        let path = super::super::pull::manifest_cache_path(&config, &pinned);
        fs::write(path, data.to_string()).await.unwrap();
        let intent = serde_json::json!({"image_ref":"busybox", "requested":requested.whole(), "pinned":pinned.whole()});
        let marker = dir.path().join("pull.pending");
        fs::write(&marker, serde_json::to_vec(&intent).unwrap()).await.unwrap();
        // Simulate interruption after the tag cache changed but before the
        // image index was updated. Restart must finish this without network.
        let tagged = super::super::pull::manifest_cache_path(&config, &requested);
        super::super::atomic_write(&tagged, data.to_string().as_bytes()).await.unwrap();
        let restarted = ImageManager::new(config);
        restarted.initialize().await.unwrap();
        for tag in ["busybox", "busybox:latest", "docker.io/library/busybox:latest"] {
            let image = restarted.get_image_status(tag).await.unwrap().unwrap();
            assert_eq!(image.image_id, next.config_digest);
        }
        assert_eq!(restarted.get_image_status("busybox:v1").await.unwrap().unwrap().image_id, facts('a').config_digest);
        assert!(!marker.exists());
    }

    #[tokio::test]
    #[ignore = "requires CAP_SYS_ADMIN and OverlayFS"]
    async fn pull_publishes_once_and_failed_unpack_leaves_index_retryable() {
        use oci_distribution::{config::ConfigFile, manifest::{OciDescriptor, OciImageManifest}, secrets::RegistryAuth, Reference};
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.storage.image_dir = temp.path().join("images");
        cfg.storage.container_dir = temp.path().join("bundles");
        let cfg = Arc::new(cfg);
        let manager = ImageManager::new(cfg.clone());
        manager.initialize().await.unwrap();
        let facts = facts('a');
        let image = format!("example.test/demo@{}", facts.manifest_digest);
        let reference: Reference = image.parse().unwrap();
        let mut manifest = OciImageManifest::default();
        manifest.config.digest = facts.config_digest.clone();
        use sha2::Digest;
        let digest = format!("sha256:{:x}", sha2::Sha256::digest([0; 1024]));
        let layer = OciDescriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar".into(),
            digest: digest.clone(),
            size: 1024,
            ..Default::default()
        };
        manifest.layers = vec![layer];
        let cached = serde_json::json!({"manifest":manifest, "config":ConfigFile::default(), "manifest_digest":facts.manifest_digest});
        fs::write(super::super::pull::manifest_cache_path(&cfg, &reference), cached.to_string()).await.unwrap();
        let layers = cfg.storage.image_dir.join("layers");
        fs::create_dir_all(&layers).await.unwrap();
        let blob = layers.join(format!("{}.blob", digest.replace(':', "-")));
        fs::write(&blob, b"not a tar archive").await.unwrap();
        assert!(manager.pull_image(&image, RegistryAuth::Anonymous, None).await.is_err());
        assert!(manager.get_image_status(&image).await.unwrap().is_none());
        assert!(!cfg.storage.image_dir.join("pull.pending").exists());

        // Repair the fixture and retry. A second pull and the first create reuse the same tree.
        fs::write(&blob, [0; 1024]).await.unwrap();
        let id = manager.pull_image(&image, RegistryAuth::Anonymous, None).await.unwrap();
        let key = super::super::snapshot::key(&id, facts.manifest_digest.as_bytes());
        let tree = cfg.storage.container_dir.join(".snapshots-v1").join(key);
        let inode = std::fs::metadata(tree.join("pin")).unwrap().ino();
        assert!(tree.join("root").is_dir());
        manager.pull_image(&image, RegistryAuth::Anonymous, None).await.unwrap();
        let bundle = cfg.storage.container_dir.join("worker");
        fs::create_dir_all(bundle.join("rootfs")).await.unwrap();
        super::super::pull::prepare_bundle(&cfg, &image, &bundle, true).await.unwrap();
        assert_eq!(std::fs::metadata(bundle.join("snapshot.v1")).unwrap().ino(), inode);
        manager.remove_image(&image, false).await.unwrap();
        assert!(tree.join("retired").exists());
        manager.pull_image(&image, RegistryAuth::Anonymous, None).await.unwrap();
        super::super::snapshot::remove(&bundle).unwrap();
        assert_eq!(std::fs::metadata(tree.join("pin")).unwrap().ino(), inode);
        assert!(!tree.join("retired").exists());
        manager.remove_image(&image, false).await.unwrap();
        assert!(!tree.exists());

        // A crash after the journal write can lose the unpublished staging directory.
        let intent = PendingPull {
            image_ref: image.clone(), requested: image.clone(), pinned: image.clone(), prepare_snapshot: true,
        };
        fs::write(cfg.storage.image_dir.join("pull.pending"), serde_json::to_vec(&intent).unwrap()).await.unwrap();
        let restarted = ImageManager::new(cfg);
        restarted.initialize().await.unwrap();
        assert!(tree.join("root").is_dir());
        assert_eq!(restarted.get_image_status(&image).await.unwrap().unwrap().image_id, id);
    }

    #[tokio::test]
    async fn removed_digest_is_not_exposed_by_internal_storage_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.image_dir = dir.path().to_owned();
        config.storage.container_dir = dir.path().join("containers");
        let store = ImageManager::new(Arc::new(config));
        store.initialize().await.unwrap();
        let first = facts('a');
        let first_ref = format!("example.test/app@{}", first.manifest_digest);
        store.record_pull(&first_ref, first).await.unwrap();
        let mut sibling = facts('a');
        sibling.manifest_digest = facts('b').manifest_digest;
        let second_ref = format!("example.test/app@{}", sibling.manifest_digest);
        store.record_pull(&second_ref, sibling).await.unwrap();
        store.remove_image(&first_ref, false).await.unwrap();
        assert!(store.get_image_status(&first_ref).await.unwrap().is_none());
        assert!(store.get_image_status(&second_ref).await.unwrap().is_some());
    }

}
