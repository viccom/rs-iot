//! gateway auth 支持 Cookie 凭证（统一会话）端到端验证。
//!
//! 两套同源 SPA（/ 控制台、/hmi 组态）共享一次登录：login 时 rs-iot 下发
//! `Set-Cookie: token=<jwt>; HttpOnly`，浏览器后续 REST/SSE 请求自动携带。
//! EventSource 不能设 header，但同源 Cookie 自动携带——SSE 免 `?token=` 的前提
//! 就是 `auth_middleware` 认 Cookie。
//!
//! 验证意图（同 auth_query_token_e2e 的对照锁死法）：cookie 里的 token 走与
//! Bearer 相同的 `verify_token` 校验——有效放行（502 反代到不存在的 rs-iot 证明
//! 过了鉴权），无 cookie / 无效 cookie / 非精确名 cookie → 401。

use std::sync::Arc;

use axum::Router;
use jsonwebtoken::{encode, EncodingKey, Header};
use rsiot_gateway::auth::Claims;
use rsiot_gateway::{build_router, GwConfig, GwState};
use tokio::net::TcpListener;

/// 起一个 axum 服务在 :0，返回实际监听 URL。
async fn spawn_axum(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://{addr}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    url
}

/// rsiot_base 指向端口 1（无监听）：鉴权通过后反代 502 = "过了鉴权"的信号。
fn test_state() -> Arc<GwState> {
    let cfg = GwConfig {
        listen: "127.0.0.1:0".into(),
        rsiot_base: "http://127.0.0.1:1".into(),
        serve_base: "http://127.0.0.1:1".into(),
        jwt_secret: "test-secret".into(),
        serve_token: "unused".into(),
        reasonix: Default::default(),
        hmi_dist: None,
        hmi_widgets_dir: None,
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

/// 仅 Cookie（无 Bearer / query）→ 鉴权放行（502 反代到不存在的 rs-iot）。
///
/// 核心断言：HttpOnly cookie 是浏览器自动携带的唯一凭证形态，必须能过鉴权。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_cookie_passes_auth() {
    let gw = spawn_axum(build_router(test_state())).await;
    let resp = reqwest::Client::new()
        .get(format!("{gw}/api/rsiot/sysstatus"))
        .header(reqwest::header::COOKIE, format!("token={}", valid_jwt()))
        .send()
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::BAD_GATEWAY,
        "valid cookie must pass auth → 502 from dead upstream (not 401)"
    );
}

/// 多对 cookie 中间夹着 token（`a=1; token=…; b=2`）也要解析正确。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cookie_token_among_other_pairs_passes_auth() {
    let gw = spawn_axum(build_router(test_state())).await;
    let resp = reqwest::Client::new()
        .get(format!("{gw}/api/rsiot/sysstatus"))
        .header(
            reqwest::header::COOKIE,
            format!("theme=dark; token={}; density=compact", valid_jwt()),
        )
        .send()
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::BAD_GATEWAY,
        "token pair among multiple cookies must be found"
    );
}

/// 无效 cookie token → 401（对照：cookie 也走 verify_token，非放任）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_cookie_token_returns_401() {
    let gw = spawn_axum(build_router(test_state())).await;
    let resp = reqwest::Client::new()
        .get(format!("{gw}/api/rsiot/sysstatus"))
        .header(reqwest::header::COOKIE, "token=garbage")
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// 空 query token（旧 bundle 拼出 `&token=`）+ 有效 Cookie → 仍放行。
///
/// 真实场景回归：升级部署后未刷新的旧控制台 bundle 读不到已迁移的旧键，
/// 拼出空 token 的 SSE URL——空值不得遮蔽同时携带的有效 cookie（否则旧标签页
/// SSE 401 死循环重连，2026-10-28 线上实测）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_query_token_falls_through_to_cookie() {
    let gw = spawn_axum(build_router(test_state())).await;
    let resp = reqwest::Client::new()
        .get(format!("{gw}/api/rsiot/api/v1/events?types=state&token="))
        .header(reqwest::header::COOKIE, format!("token={}", valid_jwt()))
        .send()
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::BAD_GATEWAY,
        "empty query token must be skipped → cookie auth passes (502 from dead upstream)"
    );
}

/// 名字仅以 token= 结尾的 cookie（如 `reasonix_token=…`）不得误当凭证——
/// 精确名解析的锁死对照（rs-iot 主 crate 旧实现 `split("token=")` 会误命中）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suffix_named_cookie_is_not_a_credential() {
    let gw = spawn_axum(build_router(test_state())).await;
    let resp = reqwest::Client::new()
        .get(format!("{gw}/api/rsiot/sysstatus"))
        .header(
            reqwest::header::COOKIE,
            format!("reasonix_token={}", valid_jwt()),
        )
        .send()
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "reasonix_token= must NOT satisfy auth (exact-name parsing)"
    );
}
