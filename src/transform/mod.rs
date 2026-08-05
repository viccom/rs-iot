//! L2 数据转换层。
//!
//! - `passthrough`：默认 Processor，原样透传
//! - `rhai`（feature `transform`）：用 rhai 脚本转换 sample.values

pub mod passthrough;
#[cfg(feature = "transform")]
pub mod rhai;
