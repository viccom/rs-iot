//! Router 组装（抽出便于集成测试，对标 rs-iot `api/mod.rs::build_router`）。
//!
//! main.rs 只做 IO 编排（读配置 + 起服务），路由逻辑在此便于测试。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::routing::{any, post};
use axum::Router;

use crate::auth::auth_middleware;
use crate::proxy::{forward, forward_agent_submit, GwState, Upstream};
use crate::routes_gw::{health, login};
use crate::static_assets;

/// 构造 Router（plan-gateway-tech-selection.md §三三类路由）：
/// - `/api/rsiot/*` → forward(Rsiot)，层2 Authorization 自动透传。
/// - `/api/agent/*` → forward(Serve)，层3 注入 cookie。
/// - `/api/gw/health` → health（PUBLIC，免 JWT）。
/// - `/api/gw/login` → login（PUBLIC，免 JWT；转发 rs-iot 验证）。
/// - 其余非 `/api/*` 路径 → `static_assets::serve`（fallback，嵌入 frontend/dist + SPA fallback）。
pub fn build_router(st: Arc<GwState>) -> Router {
    let rsiot_st = st.clone();
    let serve_st = st.clone();
    // agent 子路由用 `nest`（剥 /api/agent 前缀）而非裸显式 route——
    // 显式 route 收到完整 path，forward 会拼出带前缀的上游 URL（404 bug）。
    // `/submit` 走注入 handler（工具提示，冷启动盲区缓解），其余 fallback 裸透传。
    let agent_routes = Router::new()
        .route("/submit", post(forward_agent_submit))
        .fallback(any(move |req: Request| {
            let st = serve_st.clone();
            async move { forward(State(st), req, Upstream::Serve).await }
        }));
    Router::new()
        .route("/api/gw/health", any(health))
        .route("/api/gw/login", post(login))
        .nest("/api/agent", agent_routes)
        .nest_service(
            "/api/rsiot",
            any(move |req: Request| {
                let st = rsiot_st.clone();
                async move { forward(State(st), req, Upstream::Rsiot).await }
            }),
        )
        .fallback(static_assets::serve)
        .with_state(st.clone())
        .layer(axum::middleware::from_fn_with_state(st, auth_middleware))
}
