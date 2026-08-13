//! rsiot-gateway 二进制入口。
//!
//! v3 独立 API Gateway（plan-gateway-tech-selection.md §二架构）：
//! 前端唯一入口，纯反代 rs-iot(:8790) + reasonix serve(:8787)。
//!
//! main 只做 IO 编排（读配置 + 起服务），路由逻辑在 `lib::build_router` 便于集成测试。
//!
//! 配置：`-c <config.toml>` 或环境变量 `RSIOT_GW_CONFIG` 指向配置文件路径。

use std::sync::Arc;

use anyhow::{Context, Result};

use rsiot_gateway::{build_router, GwConfig, GwState};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cfg_path = std::env::args()
        .nth(1)
        .filter(|a| a != "-h" && a != "--help")
        .or_else(|| std::env::var("RSIOT_GW_CONFIG").ok())
        .context("missing config path; pass <config.toml> as first arg or set RSIOT_GW_CONFIG")?;
    let cfg_str =
        std::fs::read_to_string(&cfg_path).with_context(|| format!("read config {cfg_path}"))?;
    let cfg = GwConfig::parse(&cfg_str).context("parse config")?;
    let st = Arc::new(GwState::from_config(&cfg).context("build gateway state")?);

    let app = build_router(st);
    let listener = tokio::net::TcpListener::bind(&cfg.listen)
        .await
        .with_context(|| format!("bind {}", cfg.listen))?;
    tracing::info!(addr = %cfg.listen, "rsiot-gateway listening");
    axum::serve(listener, app).await.context("serve")?;
    Ok(())
}
