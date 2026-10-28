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
    let app = Router::new()
        .route("/api/gw/health", any(health))
        .route("/api/gw/login", post(login))
        .nest("/api/agent", agent_routes)
        .nest_service(
            "/api/rsiot",
            any(move |req: Request| {
                let st = rsiot_st.clone();
                async move { forward(State(st), req, Upstream::Rsiot).await }
            }),
        );
    // /hmi：hmi-studio 前端静态产物（W2，可配）。配置 hmi_dist 才挂——文件系统
    // serve + SPA fallback（static_assets::hmi_router）；未配置不挂，/hmi 落到
    // 下方既有静态 fallback。非 /api/ 前缀故免 JWT（同主前端，登录页要先加载）。
    let app = match st.hmi_dist.clone() {
        Some(dist) => app.nest_service("/hmi", static_assets::hmi_router(dist)),
        None => app,
    };
    app.fallback(static_assets::serve)
        .with_state(st.clone())
        .layer(axum::middleware::from_fn_with_state(st, auth_middleware))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GwConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// 构造测试 GwState（上游指向不可达端口无妨——/hmi 测试不打上游）。
    fn test_state(hmi_dist: Option<std::path::PathBuf>) -> Arc<GwState> {
        let cfg = GwConfig {
            listen: "127.0.0.1:0".into(),
            rsiot_base: "http://127.0.0.1:8790".into(),
            serve_base: "http://127.0.0.1:8787".into(),
            jwt_secret: "s".into(),
            serve_token: "t".into(),
            reasonix: Default::default(),
            hmi_dist,
        };
        Arc::new(GwState::from_config(&cfg).expect("state"))
    }

    /// 临时 hmi dist 目录：index.html（带测试标记）+ assets/app.js。
    fn temp_dist(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rsiot-gw-hmi-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).expect("mkdir");
        std::fs::write(dir.join("index.html"), "<html>HMI-TEST-INDEX</html>").expect("write index");
        std::fs::write(dir.join("assets").join("app.js"), "// hmi asset").expect("write asset");
        dir
    }

    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    async fn get(app: &Router, path: &str) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("call app");
        let status = resp.status();
        (status, body_text(resp).await)
    }

    /// 同 `get`，另返回 Cache-Control 头（缓存策略断言用）。
    async fn get_with_cache(app: &Router, path: &str) -> (StatusCode, String, String) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("call app");
        let status = resp.status();
        let cache = resp
            .headers()
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        (status, cache, body_text(resp).await)
    }

    /// 配置 hmi_dist 后 /hmi 三态：根路径 index、资产命中、SPA 回退。
    ///
    /// 验证意图（规则7）：index 标记 HMI-TEST-INDEX 只存在于临时 dist——命中它
    /// 即证明走的是文件系统 serve 而非 rust-embed 嵌入主前端；SPA 回退与资产
    /// 404 是 hmi-studio history 路由刷新不炸的存在性判据。
    #[tokio::test]
    async fn hmi_configured_serves_index_asset_and_spa_fallback() {
        let dist = temp_dist("mounted");
        let app = build_router(test_state(Some(dist.clone())));

        let (code, body) = get(&app, "/hmi").await;
        assert_eq!(code, StatusCode::OK, "根路径应回 dist index.html");
        assert!(body.contains("HMI-TEST-INDEX"), "got:\n{body}");

        let (code, body) = get(&app, "/hmi/assets/app.js").await;
        assert_eq!(code, StatusCode::OK, "已存在资产应原样返回");
        assert!(body.contains("hmi asset"));

        // 未命中无扩展名路径（hmi-studio 前端路由，如 /hmi/editor/line1）→ SPA 回退。
        let (code, body) = get(&app, "/hmi/editor/line1").await;
        assert_eq!(code, StatusCode::OK, "无扩展名未命中应回 index.html");
        assert!(body.contains("HMI-TEST-INDEX"));

        // 未命中带扩展名路径 → 404，不得回退 index.html（防 HTML 被当 JS 解析）。
        let (code, body) = get(&app, "/hmi/assets/missing.js").await;
        assert_eq!(code, StatusCode::NOT_FOUND, "缺失资源必须 404");
        assert!(!body.contains("HMI-TEST-INDEX"));

        let _ = std::fs::remove_dir_all(&dist);
    }

    /// /hmi 缓存策略（审查 2026-10-27）：assets/* 是 vite 哈希文件名 → immutable
    /// 一年；其余（index/SPA 回退）→ no-cache 回源校验——dist 重部署后浏览器不得
    /// 持旧 index.html 引用已删除的 hash 资产（那会表现为加载白屏直到手动刷新）。
    #[tokio::test]
    async fn hmi_cache_control_immutable_assets_and_nocache_index() {
        let dist = temp_dist("cache");
        let app = build_router(test_state(Some(dist.clone())));

        let (code, cache, _) = get_with_cache(&app, "/hmi/assets/app.js").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(cache, "public, max-age=31536000, immutable");

        let (code, cache, _) = get_with_cache(&app, "/hmi").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(cache, "no-cache");

        let (code, cache, _) = get_with_cache(&app, "/hmi/editor/line1").await;
        assert_eq!(code, StatusCode::OK, "SPA 回退");
        assert_eq!(cache, "no-cache", "SPA 回退也是 index.html，须回源校验");

        let _ = std::fs::remove_dir_all(&dist);
    }

    /// 未配置 hmi_dist → /hmi 不挂载，落到既有静态 fallback（嵌入主前端）。
    ///
    /// 验证意图（规则7）：None 是缺省态——部署未配 hmi_dist 时 gateway 行为必须
    /// 与改动前完全一致（/hmi 由主前端 SPA fallback 兜住而非 404/崩）。
    #[tokio::test]
    async fn hmi_not_configured_falls_back_to_embedded_frontend() {
        let app = build_router(test_state(None));
        let (code, body) = get(&app, "/hmi").await;
        assert_eq!(code, StatusCode::OK);
        assert!(
            body.contains("<div id=\"app\">"),
            "未配置 hmi_dist 时 /hmi 应落到嵌入主前端 index.html"
        );
        assert!(!body.contains("HMI-TEST-INDEX"));
    }

    /// /hmi 免 JWT（非 /api/ 前缀）：无凭证直接 200。
    ///
    /// 验证意图（规则7）：auth 边界是 /api/*——hmi-studio SPA 登录页要先加载
    /// 才能提交凭证，若 /hmi 要 JWT 则 SPA 启动死循环（同主前端 static_assets）。
    #[tokio::test]
    async fn hmi_does_not_require_jwt() {
        let dist = temp_dist("noauth");
        let app = build_router(test_state(Some(dist.clone())));
        // oneshot 请求不带任何 Authorization 头。
        let (code, _) = get(&app, "/hmi").await;
        assert_eq!(code, StatusCode::OK, "/hmi 必须免 JWT（同主前端）");
        let _ = std::fs::remove_dir_all(&dist);
    }
}
