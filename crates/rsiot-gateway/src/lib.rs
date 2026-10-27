//! rsiot-gateway 库入口。
//!
//! v3 独立 API Gateway：纯无状态 HTTP 反代 rs-iot + reasonix serve。
//! 设计见 `docs/decisions/2026-08-12-gateway-tech-selection.md`（axum 0.8 + reqwest 手写方案）。

// clippy 1.99：async_trait 宏为 async trait 方法生成 `#[must_use]`，返回类型本身已是
// must_use → 宏展开处必然双标记（不可编辑的生成代码）。与根 crate src/lib.rs 同款豁免。
#![allow(clippy::double_must_use)]

pub mod app;
pub mod attachments;
pub mod auth;
pub mod config;
pub mod providers;
pub mod proxy;
pub mod routes_gw;
pub mod static_assets;

pub use app::build_router;
pub use config::GwConfig;
pub use proxy::{GwState, Upstream};
