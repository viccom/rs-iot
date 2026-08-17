//! Router 组装（抽出便于集成测试，对标 rs-iot `api/mod.rs::build_router`）。
//!
//! main.rs 只做 IO 编排（读配置 + 起服务），路由逻辑在此便于测试。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::routing::{any, post};
use axum::Router;

use crate::attachments;
use crate::auth::auth_middleware;
use crate::providers;
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
        // 供应商管理（本地处理，不转发 serve——serve 无此 API，配置面由网关直管）。
        .route(
            "/providers",
            axum::routing::get(providers::list).post(providers::upsert),
        )
        .route("/providers/default", post(providers::set_default))
        .route("/providers/fetch-models", post(providers::fetch_models))
        .route(
            "/providers/{name}",
            axum::routing::delete(providers::delete),
        )
        // 附件上传 + 工作区浏览（本地处理：serve 只有 @ 解析，无落盘/浏览端点）。
        // 上传路由单独抬请求体上限（axum 默认 2MB），其余路由维持默认。
        .route(
            "/attachments",
            post(attachments::upload).layer(axum::extract::DefaultBodyLimit::max(
                attachments::UPLOAD_BODY_LIMIT,
            )),
        )
        .route("/workspace", axum::routing::get(attachments::workspace))
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
