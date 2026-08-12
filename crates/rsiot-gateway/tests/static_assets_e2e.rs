//! Task1 静态资源服务端到端测。
//!
//! 验证 gateway `build_router` fallback 到 `static_assets::serve` 的两条关键路径：
//! 1. 根路径 `/` 返回嵌入的 index.html（rust-embed 嵌入 frontend/dist）。
//! 2. SPA fallback：未命中的非文件路径（如 `/login`）回退 index.html（vue-router history 模式）。
//!
//! 另验证静态资源与 `/api/*` 反代不互相串扰（fallback 优先级低于 nest_service）——
//! 对标 health_e2e.rs 的 spawn 模式。

use std::sync::Arc;

use axum::Router;
use axum::routing::any;
use rsiot_gateway::{GwConfig, GwState, build_router};

/// 起一个 axum 服务在 :0（OS 分配端口），返回监听 URL。
async fn spawn(router: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", l.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = axum::serve(l, router).await;
    });
    url
}

/// 构造测试 GwState（上游指向不存在的端口无妨——静态资源测试不打上游）。
fn test_state() -> Arc<GwState> {
    let cfg = GwConfig {
        listen: "127.0.0.1:0".into(),
        rsiot_base: "http://127.0.0.1:8790".into(),
        serve_base: "http://127.0.0.1:8787".into(),
        jwt_secret: "s".into(),
        serve_token: "t".into(),
    };
    Arc::new(GwState::from_config(&cfg).expect("state"))
}

/// `GET /` 返回嵌入的 index.html（含占位内容），状态 200。
///
/// 验证意图（规则7）：rust-embed 在编译期把 frontend/dist/index.html 嵌入二进制，
/// fallback 必须把它吐出来——这是 gateway 作为前端入口的存在性判据。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_serves_index_html() {
    let gw = spawn(build_router(test_state())).await;
    let resp = reqwest::get(format!("{gw}/")).await.expect("request");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("placeholder"),
        "expected embedded index.html placeholder; got:\n{body}"
    );
}

/// `GET /login`（未命中的非文件路径）SPA fallback 到 index.html。
///
/// 验证意图（规则7）：vue-router history 模式要求所有非文件路径回退 index.html，
/// 由前端路由接管——否则刷新 `/login` 直接 404 破坏 SPA。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spa_fallback_returns_index_html() {
    let gw = spawn(build_router(test_state())).await;
    let resp = reqwest::get(format!("{gw}/login")).await.expect("request");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("placeholder"), "SPA fallback to index.html");
}

/// 静态资源 fallback 优先级低于 `/api/*`：访问 `/api/gw/health` 不被 fallback 拦截。
///
/// 验证意图（规则7）：fallback 是"最后兜底"，已注册的 `/api/gw/health` 路由必须优先命中。
/// 否则静态资源吞掉 API 请求，gateway 核心功能失效。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_route_not_shadowed_by_fallback() {
    // 上游指向一个 mock，rsiot health 探针会判定其在线状态。
    let mock = spawn(Router::new().route("/api/v1/sysstatus", any(|| async { "ok" }))).await;
    let cfg = GwConfig {
        listen: "127.0.0.1:0".into(),
        rsiot_base: mock.clone(),
        serve_base: mock,
        jwt_secret: "s".into(),
        serve_token: "t".into(),
    };
    let st = Arc::new(GwState::from_config(&cfg).expect("state"));
    let gw = spawn(build_router(st)).await;

    let body: serde_json::Value = reqwest::get(format!("{gw}/api/gw/health"))
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    // 拿到 JSON 响应证明走的是 health handler 而非静态 fallback（后者返 HTML）。
    assert_eq!(body["rsiot"], "online");
    assert_eq!(body["serve"], "online");
}

/// 静态资源免鉴权：`GET /` 不带 JWT 也返 200（前端登录页要先加载才能提交凭证）。
///
/// 验证意图（规则7）：auth 边界是 `/api/*`——非 API 路径必须公开，否则 SPA 启动循环
/// （没 token 看不到登录页，没登录页又拿不到 token）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_assets_do_not_require_jwt() {
    let gw = spawn(build_router(test_state())).await;
    let resp = reqwest::Client::new()
        .get(format!("{gw}/"))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 200, "static assets must be public (no JWT)");
}
