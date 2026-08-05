use thiserror::Error;

#[derive(Debug, Error)]
pub enum RsIotError {
    #[error("lux error: {0}")]
    Lux(#[from] lux::LuxError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("bcrypt error: {0}")]
    Bcrypt(#[from] bcrypt::BcryptError),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("identity error: {0}")]
    Identity(String),
}

pub type TResult<T> = std::result::Result<T, RsIotError>;
