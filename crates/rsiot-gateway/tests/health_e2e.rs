//! B1-3 gateway health 聚合端到端测。
//!
//! 验证 `GET /api/gw/health` 正确聚合 rs-iot + serve 的在线状态（plan-gateway-tech-selection.md
//! §七 B1-3）。health 在 PUBLIC_PATHS 免 JWT，是 gateway 自有业务端点（Pingora 硬伤处）。

use std::sync::Arc;

use axum::routing::any;
use axum::Router;
use rsiot_gateway::{build_router, GwConfig, GwState};

/// 起一个 axum 服务在 :0，返回实际监听 URL。
async fn spawn_axum(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let url = format!("http://{addr}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    url
}

/// 起一个总是返 200 的 mock（模拟在线上游）。
fn ok_router() -> Router {
    Router::new().route("/api/v1/sysstatus", any(|| async { "ok" }))
}

/// 构造测试 GwState，指向两个 mock URL。
fn test_state(rsiot_base: String, serve_base: String) -> Arc<GwState> {
    let cfg = GwConfig {
        listen: "127.0.0.1:0".into(),
        rsiot_base,
        serve_base,
        jwt_secret: "test-secret".into(),
        serve_token: "tok".into(),
        reasonix: Default::default(),
    };
    Arc::new(GwState::from_config(&cfg).expect("state"))
}

/// 两个上游都在线 → health 返 {"rsiot":"online","serve":"online"}。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_both_online() {
    let mock = spawn_axum(ok_router()).await;
    let gw = spawn_axum(build_router(test_state(mock.clone(), mock))).await;

    let resp = reqwest::Client::new()
        .get(format!("{gw}/api/gw/health"))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["rsiot"], "online");
    assert_eq!(body["serve"], "online");
    assert_eq!(body["mcp"], "unknown"); // MCP 占位，gateway 视角不可见
}

/// rs-iot 离线（指向不存在的端口）+ serve 在线 → 混合状态。
///
/// 验证意图（规则7）：聚合探针必须独立判定每个上游——一个挂不能误判另一个。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_mixed_when_one_upstream_down() {
    let serve_up = spawn_axum(ok_router()).await;
    // rsiot 指向一个未监听的端口（OS 分配一个空闲端口但不 bind → 连接拒绝）。
    let rsiot_down = "http://127.0.0.1:1";
    let gw = spawn_axum(build_router(test_state(rsiot_down.into(), serve_up))).await;

    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{gw}/api/gw/health"))
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    assert_eq!(body["rsiot"], "offline", "rsiot down detected");
    assert_eq!(body["serve"], "online", "serve still up");
}

/// health 免 JWT（PUBLIC_PATHS）——不带 token 也能访问。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_is_public_no_jwt_required() {
    let mock = spawn_axum(ok_router()).await;
    let gw = spawn_axum(build_router(test_state(mock.clone(), mock))).await;

    let resp = reqwest::Client::new()
        .get(format!("{gw}/api/gw/health"))
        .send()
        .await
        .expect("request");
    // 免 JWT：不带 Authorization 头仍 200（非 401）。
    assert_eq!(resp.status(), 200, "health must be in PUBLIC_PATHS");
}
