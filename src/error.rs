//! rs-iot 库错误类型。
//!
//! 库的公开 API 统一返回 `Result<_, RsIotError>`；binary 层（main/run/service）
//! 仍可用 `anyhow` 做带上下文的错误聚合。

use thiserror::Error;

/// rs-iot 库错误。
///
/// 覆盖 lux / io / bcrypt / json / 配置 / 身份等来源；STEP 2 将补 `Toml`、`Mqtt` 变体并加 `#[non_exhaustive]`。
#[derive(Debug, Error)]
pub enum RsIotError {
    /// lux 嵌入数据库错误。
    #[error("lux error: {0}")]
    Lux(#[from] lux::LuxError),

    /// 标准 IO 错误。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// bcrypt 口令哈希错误。
    #[error("bcrypt error: {0}")]
    Bcrypt(#[from] bcrypt::BcryptError),

    /// JSON 序列化/反序列化错误。
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// 配置错误（语义级）。
    #[error("config error: {0}")]
    Config(String),

    /// 身份/引导错误。
    #[error("identity error: {0}")]
    Identity(String),
}

/// 库统一 Result 别名。
pub type TResult<T> = std::result::Result<T, RsIotError>;
