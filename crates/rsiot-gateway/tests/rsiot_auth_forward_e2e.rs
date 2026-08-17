//! C1 修复的端到端验证：gateway 转发 rsiot 时，query token 必须转为 Authorization 凭证。
//!
//! 审查 finding（proxy.rs:117）：EventSource 不能设 header，前端只能 `?token=<jwt>`。
//! gateway auth_middleware 验 query token 通过，但 forward() 对 Upstream::Rsiot 不注入
//! 任何凭证，而 rs-iot auth_middleware 只认 Authorization/Cookie 不认 query → SSE 401。
//!
//! 既有 auth_query_token_e2e.rs 用死端口 :1 验 502，**rsiot 鉴权逻辑根本没走到**（假绿）。
//! 本测试起**真实 mock rsiot**（校验 Authorization: Bearer），堵住该盲区：
//! - query token 经 gateway 转发后，mock rsiot 必须收到有效 Authorization → 200
//! - 转发的 upstream URL 不得含 token=（防泄漏，修法 B 副作用）
//!
//! 验证意图（规则7）：query token 不只是"被 gateway 读取"，还要"被正确翻译成 rsiot 认得的
//! 凭证传递下去"。若 forward 退化成只透传原请求（无凭证注入），mock rsiot 会 401。

use std::sync::{Arc, Mutex};

use axum::{routing::any, Router};
use rsiot_gateway::auth::Claims;
use rsiot_gateway::{build_router, GwConfig, GwState};
use tokio::net::TcpListener;

/// mock rsiot 记录收到的请求（路径 + 是否带 Authorization）。
#[derive(Default, Clone)]
struct RecordedReq {
    got_auth: bool,
    path: String,
}

/// 起 mock rsiot：校验 Authorization: Bearer，记录请求，返 200/401。
async fn spawn_mock_rsiot(recorded: Arc<Mutex<RecordedReq>>) -> String {
    let app = Router::new().route(
        "/api/v1/events",
        any(|req: axum::extract::Request| async move {
            let has_auth = req
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|s| s.starts_with("Bearer "));
            let path = req
                .uri()
                .path_and_query()
                .map(|s| s.to_string())
                .unwrap_or_default();
            *recorded.lock().unwrap() = RecordedReq {
                got_auth: has_auth,
                path,
            };
            if has_auth {
                (axum::http::StatusCode::OK, "ok")
            } else {
                (axum::http::StatusCode::UNAUTHORIZED, "no auth")
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind rsiot");
    let addr = listener.local_addr().expect("addr");
    let url = format!("http://{addr}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    url
}

async fn spawn_axum(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind gw");
    let addr = listener.local_addr().expect("addr");
    let url = format!("http://{addr}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    url
}

fn valid_jwt() -> String {
    let claims = Claims {
        sub: "test-user".into(),
        exp: (chrono::Local::now().timestamp() as usize) + 3600,
    };
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(b"test-secret"),
    )
    .expect("encode jwt")
}

/// query token 经 gateway 转发，mock rsiot 必须收到 Authorization: Bearer → 200。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_token_forwarded_as_authorization() {
    let recorded = Arc::new(Mutex::new(RecordedReq::default()));
    let rsiot_url = spawn_mock_rsiot(recorded.clone()).await;

    let cfg = GwConfig {
        listen: "127.0.0.1:0".into(),
        rsiot_base: rsiot_url,
        serve_base: "http://127.0.0.1:1".into(),
        jwt_secret: "test-secret".into(),
        serve_token: "unused".into(),
        reasonix: Default::default(),
    };
    let gw = spawn_axum(build_router(Arc::new(
        GwState::from_config(&cfg).expect("state"),
    )))
    .await;

    let token = valid_jwt();
    let resp = reqwest::get(format!(
        "{gw}/api/rsiot/api/v1/events?types=log&token={token}"
    ))
    .await
    .expect("request");

    // 核心：mock rsiot 必须收到 Authorization → 200（不是 401）。
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "query token must be translated to Authorization when forwarded to rsiot"
    );
    let rec = recorded.lock().unwrap().clone();
    assert!(
        rec.got_auth,
        "mock rsiot must receive Authorization: Bearer"
    );
    // 防 token 泄漏：转发的上游 URL 不得含 token=。
    assert!(
        !rec.path.contains("token="),
        "upstream URL must not leak token= (got: {})",
        rec.path
    );
    assert!(
        rec.path.contains("types=log"),
        "business query must be preserved (got: {})",
        rec.path
    );
}

/// 对照：query token 无效 → gateway 自己 401（不走到 rsiot）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_query_token_blocked_at_gateway() {
    let recorded = Arc::new(Mutex::new(RecordedReq::default()));
    let rsiot_url = spawn_mock_rsiot(recorded.clone()).await;
    let cfg = GwConfig {
        listen: "127.0.0.1:0".into(),
        rsiot_base: rsiot_url,
        serve_base: "http://127.0.0.1:1".into(),
        jwt_secret: "test-secret".into(),
        serve_token: "unused".into(),
        reasonix: Default::default(),
    };
    let gw = spawn_axum(build_router(Arc::new(
        GwState::from_config(&cfg).expect("state"),
    )))
    .await;

    let resp = reqwest::get(format!("{gw}/api/rsiot/api/v1/events?token=garbage"))
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "invalid token must be blocked at gateway auth layer"
    );
}
