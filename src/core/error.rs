#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Configuration error: {0}")]
    Config(String),
    
    #[error("Runtime error: {0}")]
    Runtime(String),
    
    #[error("Storage error: {0}")]
    Storage(String),
    
    #[error("Network error: {0}")]
    Network(String),
    
    #[error("CRI error: {0}")]
    Cri(String),
    
    #[error("Container error: {0}")]
    Container(String),
    
    #[error("Pod error: {0}")]
    Pod(String),
    
    #[error("Image error: {0}")]
    Image(String),
    
    #[error("Monitoring error: {0}")]
    Monitoring(String),
    
    #[error("Security error: {0}")]
    Security(String),
    
    #[error("Performance error: {0}")]
    Performance(String),
    
    #[error("Experimental error: {0}")]
    Experimental(String),
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    
    #[error("TOML error: {0}")]
    Toml(#[from] toml::de::Error),
    
    #[error("Database error: {0}")]
    Database(#[from] sled::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

// Convert our Error to tonic::Status for gRPC responses
impl From<Error> for tonic::Status {
    fn from(err: Error) -> Self {
        match err {
            Error::Config(msg) => tonic::Status::invalid_argument(msg),
            Error::Runtime(msg) => tonic::Status::internal(msg),
            Error::Storage(msg) => tonic::Status::internal(msg),
            Error::Network(msg) => tonic::Status::unavailable(msg),
            Error::Cri(msg) => tonic::Status::internal(msg),
            Error::Container(msg) => tonic::Status::not_found(msg),
            Error::Pod(msg) => tonic::Status::not_found(msg),
            Error::Image(msg) => tonic::Status::internal(msg),
            Error::Monitoring(msg) => tonic::Status::internal(msg),
            Error::Security(msg) => tonic::Status::permission_denied(msg),
            Error::Performance(msg) => tonic::Status::resource_exhausted(msg),
            Error::Experimental(msg) => tonic::Status::unimplemented(msg),
            Error::Io(err) => tonic::Status::internal(format!("I/O error: {}", err)),
            Error::Serialization(err) => tonic::Status::internal(format!("Serialization error: {}", err)),
            Error::Toml(err) => tonic::Status::internal(format!("TOML error: {}", err)),
            Error::Database(err) => tonic::Status::internal(format!("Database error: {}", err)),
        }
    }
}