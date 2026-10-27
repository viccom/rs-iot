//! L2 数据转换层。
//!
//! - `passthrough`：默认 Processor，原样透传
//! - `rhai`（feature `transform`）：用 rhai 脚本转换 sample.values

/// 默认透传 Processor。
pub mod passthrough;
/// rhai 脚本 Processor（`transform` feature）。
#[cfg(feature = "transform")]
pub mod rhai;
