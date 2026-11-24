pub mod container;
pub mod pod;

pub use container::ContainerManager;
pub use pod::PodManager;


pub mod vm_log;

#[derive(Default)]
pub(crate) struct OperationLocks(std::sync::Mutex<std::collections::HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>);

impl OperationLocks {
    pub async fn lock(&self, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut map = self.0.lock().unwrap();
            map.retain(|_, lock| lock.strong_count() > 0);
            let entry = map.entry(id.to_owned()).or_default();
            entry.upgrade().unwrap_or_else(|| {
                let new = std::sync::Arc::new(tokio::sync::Mutex::new(()));
                *entry = std::sync::Arc::downgrade(&new);
                new
            })
        };
        lock.lock_owned().await
    }
}
