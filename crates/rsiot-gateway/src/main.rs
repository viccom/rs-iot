//! rsiot-gateway 二进制入口。
//!
//! v3 独立 API Gateway（plan-gateway-tech-selection.md §二架构）：
//! 前端唯一入口，纯反代 rs-iot(:8790) + reasonix serve(:8787)。
//!
//! 配置：`-c <config.toml>` 或环境变量 `RSIOT_GW_CONFIG` 指向配置文件路径。

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::routing::any;
use axum::Router;

use rsiot_gateway::{
    auth::auth_middleware,
    config::GwConfig,
    proxy::{forward, GwState, Upstream},
    routes_gw::health,
};

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
    let cfg_str = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("read config {cfg_path}"))?;
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

/// 构造 Router（抽出便于集成测试）。
///
/// 路由三类（plan-gateway-tech-selection.md §三）：
/// - `/api/rsiot/*` → forward(Rsiot)，层2 Authorization 自动透传。
/// - `/api/agent/*` → forward(Serve)，层3 注入 cookie。
/// - `/api/gw/health` → health（PUBLIC，免 JWT）。
pub fn build_router(st: Arc<GwState>) -> Router {
    let rsiot_st = st.clone();
    let serve_st = st.clone();
    Router::new()
        .route("/api/gw/health", any(health))
        .nest_service(
            "/api/rsiot",
            any(move |req: Request| {
                let st = rsiot_st.clone();
                async move { forward(State(st), req, Upstream::Rsiot, "/api/rsiot").await }
            }),
        )
        .nest_service(
            "/api/agent",
            any(move |req: Request| {
                let st = serve_st.clone();
                async move { forward(State(st), req, Upstream::Serve, "/api/agent").await }
            }),
        )
        .with_state(st.clone())
        .layer(axum::middleware::from_fn_with_state(
            st,
            auth_middleware,
        ))
}
