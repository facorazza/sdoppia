use thiserror::Error;

#[derive(Error, Debug)]
pub enum DedupError {
    #[error("Failed to hash file '{path}': {source}")]
    HashFile {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Invalid directory path: {path}")]
    InvalidPath { path: String },

    #[error("Failed to read file metadata for '{path}': {source}")]
    Metadata {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Failed to get modification time for '{path}': {source}")]
    ModificationTime {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Channel closed unexpectedly")]
    ChannelClosed,

    #[error("Database operation failed: {0}")]
    Database(#[from] sqlx::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("System time error: {0}")]
    SystemTime(#[from] std::time::SystemTimeError),
}

pub type Result<T> = std::result::Result<T, DedupError>;
