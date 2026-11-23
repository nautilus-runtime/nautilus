use nautilus::core::Config;
use nautilus::storage::ImageManager;
use std::sync::Arc;
use tokio;

#[tokio::test]
async fn test_image_manager_creation() {
    let config = Arc::new(Config::default());
    let _image_manager = ImageManager::new(config);
    
    // Test that image manager can be created
    assert!(true); // Basic creation test
}

#[tokio::test]
async fn test_image_listing() {
    let config = Arc::new(Config::default());
    let image_manager = ImageManager::new(config);
    
    // Test image listing (should work even without real images)
    let result = image_manager.list_images(None).await;
    
    // In a test environment, this might fail if podman is not available
    // but the function should at least be callable
    match result {
        Ok(images) => {
            println!("Found {} images", images.len());
            // Images vector length is always non-negative by definition
        }
        Err(e) => panic!("image listing failed: {e}"),
    }
}

#[tokio::test]
async fn test_image_status() {
    let config = Arc::new(Config::default());
    let image_manager = ImageManager::new(config);
    
    // Test getting status of a non-existent image
    let result = image_manager.get_image_status("non-existent-image").await;
    
    match result {
        Ok(status) => {
            // Should return None for non-existent image
            assert!(status.is_none());
        }
        Err(e) => panic!("image status check failed: {e}"),
    }
}

#[tokio::test]
async fn test_filesystem_info() {
    let config = Arc::new(Config::default());
    let image_manager = ImageManager::new(config);
    
    // Test filesystem info
    let result = image_manager.get_fs_info().await;
    
    match result {
        Ok((total, used)) => {
            println!("Filesystem: {} total, {} used", total, used);
            assert!(total >= used); // Total should be >= used
        }
        Err(e) => panic!("filesystem info failed: {e}"),
    }
}