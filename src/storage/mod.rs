pub mod image;
pub mod pull;
pub mod snapshot;
pub mod unpack;
pub(crate) mod user;

pub use image::ImageManager;

impl From<pathrs::error::Error> for crate::core::Error {
    fn from(err: pathrs::error::Error) -> Self {
        Self::Image(format!("rootfs operation: {err}"))
    }
}

pub(super) async fn atomic_write(path: &std::path::Path, data: &[u8]) -> crate::core::Result<()> {
    use tokio::io::AsyncWriteExt;
    let dir = path.parent().expect("image store file has a parent");
    let (file, tmp) = tempfile::NamedTempFile::new_in(dir)?.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    file.write_all(data).await?;
    file.flush().await?;
    tmp.persist(path).map_err(|err| err.error)?;
    Ok(())
}
