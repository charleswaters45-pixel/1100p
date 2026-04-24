use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("transport I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("RPC error {code}: {message}")]
    Rpc { code: i64, message: String },

    #[error("protocol: {0}")]
    Protocol(String),

    #[error("server not initialized")]
    NotInitialized,

    #[error("tool call cancelled")]
    Cancelled,

    #[error("connection closed")]
    Closed,

    #[cfg(feature = "http")]
    #[error("HTTP: {0}")]
    Http(#[from] hyper::Error),

    #[cfg(feature = "http")]
    #[error("HTTP client: {0}")]
    HttpClient(#[from] hyper_util::client::legacy::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
