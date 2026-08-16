//! rsiot-gateway 库入口。
//!
//! v3 独立 API Gateway：纯无状态 HTTP 反代 rs-iot + reasonix serve。
//! 设计见 `docs/plan-gateway-tech-selection.md`（axum 0.8 + reqwest 手写方案）。

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
