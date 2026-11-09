//! rsiot-mcp 库入口。
//!
//! 作为 reasonix 的 `[[plugins]]` 子进程被 spawn，经 stdio NDJSON 通信，
//! 内部把 MCP 工具调用翻译成 rs-iot 主进程的 loopback REST 请求。
//!
//! 设计照 `G:\AI_Agent\dbx\crates\dbx-mcp` 范本，砍掉 dbx 的
//! session/transport/paths/sql_risk/mongo/redis/LocalBackend（rs-iot 用不上），
//! 只保留 server（工具声明 + 三档 risk）+ backend（WebBackend JWT 版）+ main（stdio 入口）。

// async-trait 0.1.91 宏展开给 `async fn` 自动加 `#[must_use]`，rustc 1.99 的
// clippy 判定为 double_must_use；待 async-trait 上游适配后移除（同主 crate）。
#![allow(clippy::double_must_use)]

pub mod backend;
pub mod server;

pub use backend::{BackendError, RsiotBackend, WebBackend};
pub use server::{RiskPolicy, RsiotMcpServer};
