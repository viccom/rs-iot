//! rs-iot 库错误类型。
//!
//! 库的公开 API 统一返回 `Result<_, RsIotError>`；binary 层（main/run/service）
//! 仍可用 `anyhow` 做带上下文的错误聚合（`RsIotError` 实现 `std::error::Error`，
//! 可被 `anyhow::Error` 自动收纳）。

use thiserror::Error;

/// rs-iot 库错误。
///
/// 覆盖 lux / io / bcrypt / json / toml / mqtt / 配置 / 身份等来源。
/// `#[non_exhaustive]` 允许未来新增变体而不破坏下游的 `match`。
#[derive(Debug, Error)]
#[non_exhaustive]
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

    /// TOML 反序列化错误。
    #[error("toml error: {0}")]
    Toml(#[from] toml::de::Error),

    /// MQTT 客户端错误（rumqttc）。
    #[error("mqtt error: {0}")]
    Mqtt(#[from] rumqttc::ClientError),

    /// JWT 签发/验证错误（jsonwebtoken）。
    #[error("jwt error: {0}")]
    Jsonwebtoken(#[from] jsonwebtoken::errors::Error),

    /// 配置错误（语义级，含路径等上下文）。
    #[error("config error: {0}")]
    Config(String),

    /// 身份/引导错误。
    #[error("identity error: {0}")]
    Identity(String),

    /// 实例唯一启动守卫：同 inst_id 已处于活跃态（starting/running/stopping），
    /// 拒绝二次启动（REST 映射 409；Stopped/Failed 放行——stop 后可重启）。
    #[error("instance {0} already active (starting/running/stopping)")]
    InstanceAlreadyRunning(String),

    /// 设备模型/注册表校验错误（结构或引用完整性）。
    #[error("model error: {0}")]
    Model(#[from] ModelError),

    /// 历史引擎初始化/关闭失败（tsink 错误串化）。
    ///
    /// 不用 `#[from]`：tsink 是 `history` feature 下的可选依赖，其错误类型
    /// 不进公共错误面；调用点手工 `RsIotError::HistoryEngine(e.to_string())`。
    #[error("history engine error: {0}")]
    HistoryEngine(String),
}

/// 库统一 Result 别名。
pub type TResult<T> = std::result::Result<T, RsIotError>;

// ---------- 模型/设备校验错误 ----------
//
// 原定义在 `model::validate`，被 `RsIotError::Model(#[from] …)` 引用，形成
// `error <-> model` 模块环（error 是底层，却依赖域模块）。2026-11-01 分层耦合
// 审查把类型上移到本模块——`model::validate` 保留 `pub use` 兼容既有路径。
// 判据同 `InstanceAlreadyRunning`：进入本 crate 错误分类法的域错误，归此一处。

/// 模型/设备校验错误（结构 + 引用两族；`#[non_exhaustive]` 允许后续期新增变体）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModelError {
    /// id/tag 名为空或含非法字符（空白与 `. * { } ,`——分别与 tag_key 层级、
    /// glob 通配、地址占位符语法、wire 串接冲突）。
    InvalidId {
        /// 语义名（"model_id"/"devid"/"tag"…）。
        what: &'static str,
        /// 非法值。
        value: String,
    },
    /// tag 名模型内重复。
    DuplicateTag {
        /// 模型 id。
        model: String,
        /// 重复 tag 名。
        tag: String,
    },
    /// 模板 addr 占位符未在模型 params 声明。
    UndeclaredParam {
        /// 模型 id。
        model: String,
        /// tag 名。
        tag: String,
        /// 未声明的参数名。
        param: String,
    },
    /// 模板 addr 本身非法（未闭合括号等；转 `model::addr::AddrError` 文案）。
    BadAddr {
        /// 模型 id。
        model: String,
        /// tag 名。
        tag: String,
        /// 非法原因。
        reason: String,
    },
    /// devreg 引用的模型不存在（store 写入时查表）。
    ModelNotFound {
        /// 设备 id。
        devid: String,
        /// 引用的模型 id。
        model: String,
    },
    /// 设备 params 缺模型声明的参数（齐套校验：模型 params ⊆ 设备 params）。
    MissingDeviceParam {
        /// 设备 id。
        devid: String,
        /// 缺失参数名。
        param: String,
    },
    /// 设备覆盖的 tag 不在模型 tags。
    UnknownTagOverride {
        /// 设备 id。
        devid: String,
        /// 覆盖键（tag 名）。
        tag: String,
    },
    /// extra 扩展槽键违反命名契约（设计 §4.1：`^x_[A-Za-z0-9_]+$`）。
    BadExtraKey {
        /// 模型 id。
        model: String,
        /// 所属 tag 名（模型级 extra 无 tag → None）。
        tag: Option<String>,
        /// 违规键名。
        key: String,
    },
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidId { what, value } => {
                write!(
                    f,
                    "{what} {value:?} 非法：须非空且不含空白与 . * {{ }} , 字符"
                )
            }
            Self::DuplicateTag { model, tag } => write!(f, "模型 {model} 的 tag {tag:?} 重复"),
            Self::UndeclaredParam { model, tag, param } => write!(
                f,
                "模型 {model} 的 tag {tag:?} 地址引用了未声明参数 {{{param}}}"
            ),
            Self::BadAddr { model, tag, reason } => {
                write!(f, "模型 {model} 的 tag {tag:?} 地址模板非法：{reason}")
            }
            Self::ModelNotFound { devid, model } => {
                write!(f, "设备 {devid} 引用的模型 {model:?} 不存在")
            }
            Self::MissingDeviceParam { devid, param } => {
                write!(f, "设备 {devid} 缺模型声明参数 {{{param}}}")
            }
            Self::UnknownTagOverride { devid, tag } => {
                write!(f, "设备 {devid} 覆盖的 tag {tag:?} 不在其模型 tags")
            }
            Self::BadExtraKey { model, tag, key } => match tag {
                Some(tag) => write!(
                    f,
                    "模型 {model} 的 tag {tag:?} 扩展键 {key:?} 非法：须匹配 ^x_[A-Za-z0-9_]+$"
                ),
                None => write!(
                    f,
                    "模型 {model} 的扩展键 {key:?} 非法：须匹配 ^x_[A-Za-z0-9_]+$"
                ),
            },
        }
    }
}

impl std::error::Error for ModelError {}
