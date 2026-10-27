//! rsiot-mcp 库入口。
//!
//! 作为 reasonix 的 `[[plugins]]` 子进程被 spawn，经 stdio NDJSON 通信，
//! 内部把 MCP 工具调用翻译成 rs-iot 主进程的 loopback REST 请求。
//!
//! 设计照 `G:\AI_Agent\dbx\crates\dbx-mcp` 范本，砍掉 dbx 的
//! session/transport/paths/sql_risk/mongo/redis/LocalBackend（rs-iot 用不上），
//! 只保留 server（工具声明 + 三档 risk）+ backend（WebBackend JWT 版）+ main（stdio 入口）。

// clippy 1.99：async_trait 宏为 async trait 方法生成 `#[must_use]`，返回类型本身已是
// must_use → 宏展开处必然双标记（不可编辑的生成代码）。与根 crate src/lib.rs 同款豁免。
#![allow(clippy::double_must_use)]

pub mod backend;
pub mod server;

pub use backend::{BackendError, RsiotBackend, WebBackend};
pub use server::{RiskPolicy, RsiotMcpServer};
