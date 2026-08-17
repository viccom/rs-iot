//! Task 2：gateway auth 支持 query token（EventSource 鉴权）端到端验证。
//!
//! EventSource 标准 API 不能设 HTTP header（`Authorization`/`Cookie` 均不可），
//! 只能把凭证放 URL query。监控 SSE（`/api/rsiot/events`）+ AI 面板 SSE
//! （`/api/agent/events`）都需鉴权，故 `auth_middleware` 必须支持 `?token=<jwt>`。
//!
//! 验证意图（规则7）：query token 不仅要"被读取"，还要走与 Authorization header 同样的
//! `verify_token` 校验——有效 token 放行（502 反代到不存在的 rs-iot，证明过了鉴权），
//! 无 token / 无效 token → 401。对照组锁死语义边界，避免"query token 静默忽略"的退化。
//!
//! 架构：build_router 起在 :0，rsiot_base 指向端口 1（loopback 无监听 → connect refused
//! → forward 返 502）。502 是"鉴权已放行"的可观测信号——若 auth 没放行会先返 401。

use std::sync::Arc;

use axum::Router;
use jsonwebtoken::{encode, EncodingKey, Header};
use rsiot_gateway::auth::Claims;
use rsiot_gateway::{build_router, GwConfig, GwState};
use tokio::net::TcpListener;

/// 起一个 axum 服务在 :0，返回实际监听 URL。
///
/// 测试结束 drop join_handle 即停服务（axum::serve 的 future 被 drop 中止）。
async fn spawn_axum(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://{addr}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    url
}

/// 构造测试 state：rsiot_base 指向端口 1（loopback 无监听），保证鉴权通过后反代返回 502
/// （502 = "过了鉴权"的可观测信号；若 auth 没放行会先返 401）。
fn test_state() -> Arc<GwState> {
    let cfg = GwConfig {
        listen: "127.0.0.1:0".into(),
        // 端口 1 是特权端口，本机不会有监听 → connect refused → forward 返 502。
        rsiot_base: "http://127.0.0.1:1".into(),
        serve_base: "http://127.0.0.1:1".into(),
        jwt_secret: "test-secret".into(),
        serve_token: "unused".into(),
        reasonix: Default::default(),
    };
    Arc::new(GwState::from_config(&cfg).expect("build state"))
}

/// 签发有效 JWT（与 gateway auth.rs `Claims` 同结构，共享 test-secret）。
fn valid_jwt() -> String {
    let claims = Claims {
        sub: "test-user".into(),
        exp: (chrono::Local::now().timestamp() as usize) + 3600,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(b"test-secret"),
    )
    .expect("encode jwt")
}

/// 有效 JWT 走 query 参数 → 鉴权放行（502 反代到不存在的 rs-iot）。
///
/// 这是核心断言：EventSource 不能设 header，必须 `?token=<jwt>` 能通过鉴权。
/// 502 而非 401 证明 auth_middleware 读了 query token 并验通过。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_query_token_passes_auth() {
    let gw = spawn_axum(build_router(test_state())).await;
    let token = valid_jwt();
    // /api/rsiot/sysstatus 是非 PUBLIC 的需鉴权端点。
    let resp = reqwest::get(format!("{gw}/api/rsiot/sysstatus?token={token}"))
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::BAD_GATEWAY,
        "valid query token must pass auth → 502 from dead upstream (not 401)"
    );
}

/// 无 token → 401（对照组 1：证明非 PUBLIC 端点确实要鉴权）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_token_returns_401() {
    let gw = spawn_axum(build_router(test_state())).await;
    let resp = reqwest::get(format!("{gw}/api/rsiot/sysstatus"))
        .await
        .expect("request");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// 无效 query token → 401（对照组 2：证明 query token 走 verify_token 校验，
/// 不是放任任意值通过——防止"query token 被读取但未校验"的退化实现）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_query_token_returns_401() {
    let gw = spawn_axum(build_router(test_state())).await;
    let resp = reqwest::get(format!("{gw}/api/rsiot/sysstatus?token=garbage"))
        .await
        .expect("request");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// Authorization header 仍优先工作（回归：加 query 分支不破坏既有 header 路径）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn header_token_still_works() {
    let gw = spawn_axum(build_router(test_state())).await;
    let resp = reqwest::Client::new()
        .get(format!("{gw}/api/rsiot/sysstatus"))
        .bearer_auth(valid_jwt())
        .send()
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::BAD_GATEWAY,
        "header auth must still pass (502 from dead upstream)"
    );
}
