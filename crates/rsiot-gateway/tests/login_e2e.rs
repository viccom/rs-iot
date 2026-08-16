//! Task 3 gateway login 转发端到端测。
//!
//! 验证 `POST /api/gw/login` 转发 rs-iot `/api/v1/user/login` 并透传返的 JWT
//!（plan-ai-agent-integration M1）。login 在 PUBLIC_PATHS 免 JWT，gateway 不验
//! 密码（rs-iot 验 bcrypt），纯转发。jwt_secret 与 rs-iot 同值，故 rs-iot 颁的
//! JWT gateway 也认。

use std::sync::Arc;

use axum::routing::post;
use axum::{Json, Router};
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

/// 构造测试 GwState，指向 mock rs-iot + 任意 serve（login 不探 serve）。
fn test_state(rsiot_base: String) -> Arc<GwState> {
    let cfg = GwConfig {
        listen: "127.0.0.1:0".into(),
        rsiot_base,
        serve_base: "http://127.0.0.1:8787".into(),
        jwt_secret: "s".into(),
        serve_token: "t".into(),
        reasonix: Default::default(),
    };
    Arc::new(GwState::from_config(&cfg).expect("state"))
}

/// login 转发 rs-iot 验证：mock rs-iot 返 {token}，gateway 透传给前端。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_forwards_to_rsiot_and_returns_token() {
    // mock rs-iot: /api/v1/user/login 返 {token}。
    let mock_rsiot = spawn_axum(Router::new().route(
        "/api/v1/user/login",
        post(|| async { Json(serde_json::json!({"status":"ok","token":"jwt-from-rsiot"})) }),
    ))
    .await;

    let gw = spawn_axum(build_router(test_state(mock_rsiot))).await;

    let resp: serde_json::Value = reqwest::Client::new()
        .post(format!("{gw}/api/gw/login"))
        .json(&serde_json::json!({"user_id":"admin","pass":"admin"}))
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    assert_eq!(resp["token"], "jwt-from-rsiot");
}

/// login 免 JWT（PUBLIC_PATHS）——不带 Authorization 头也能访问。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_is_public_no_jwt_required() {
    let mock_rsiot = spawn_axum(Router::new().route(
        "/api/v1/user/login",
        post(|| async { Json(serde_json::json!({"status":"ok","token":"t"})) }),
    ))
    .await;
    let gw = spawn_axum(build_router(test_state(mock_rsiot))).await;

    let resp = reqwest::Client::new()
        .post(format!("{gw}/api/gw/login"))
        .json(&serde_json::json!({"user_id":"admin","pass":"admin"}))
        .send()
        .await
        .expect("request");
    // 免 JWT：不带 Authorization 头仍 200（非 401）。
    assert_eq!(resp.status(), 200, "login must be in PUBLIC_PATHS");
}
