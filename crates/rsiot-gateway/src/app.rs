//! Router 组装（抽出便于集成测试，对标 rs-iot `api/mod.rs::build_router`）。
//!
//! main.rs 只做 IO 编排（读配置 + 起服务），路由逻辑在此便于测试。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::routing::any;
use axum::Router;

use crate::auth::auth_middleware;
use crate::proxy::{forward, GwState, Upstream};
use crate::routes_gw::health;

/// 构造 Router（plan-gateway-tech-selection.md §三三类路由）：
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
                async move { forward(State(st), req, Upstream::Rsiot).await }
            }),
        )
        .nest_service(
            "/api/agent",
            any(move |req: Request| {
                let st = serve_st.clone();
                async move { forward(State(st), req, Upstream::Serve).await }
            }),
        )
        .with_state(st.clone())
        .layer(axum::middleware::from_fn_with_state(st, auth_middleware))
}
