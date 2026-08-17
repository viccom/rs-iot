//! agent submit 工具提示注入端到端验证。
//!
//! 冷启动盲区根因(记忆 serve-coldstart-blindspot):模型首轮看不到/不愿查 MCP 工具
//! (capability list 284KB 被截断),退回 bash 直连 redis-cli。解法(用户约束:主 crate
//! 零改动 + 不改 reasonix 源码):gateway 在转发 `POST /api/agent/submit` 时,把工具
//! 提示前缀注入 body 的 `input` 字段——模型每轮都被告知 rsiot MCP 工具存在与用途。
//!
//! 验证意图(规则7):注入必须"只"发生在 submit 路径(input 前缀拼提示,原文本保留),
//! 其他 /api/agent/* 路径仍裸透传(不注入)。若注入破坏 JSON 或误伤其他端点,前端/模型
//! 都会出错。

use std::sync::Arc;

use axum::body::to_bytes;
use axum::routing::any;
use axum::Router;
use jsonwebtoken::{encode, EncodingKey, Header};
use rsiot_gateway::auth::Claims;
use rsiot_gateway::{build_router, GwConfig, GwState};
use tokio::net::TcpListener;

/// 起一个 axum 服务在 :0，返回监听 URL。
async fn spawn_axum(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://{addr}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    url
}

fn test_state(serve_base: String) -> Arc<GwState> {
    let cfg = GwConfig {
        listen: "127.0.0.1:0".into(),
        rsiot_base: "http://127.0.0.1:1".into(),
        serve_base,
        jwt_secret: "test-secret".into(),
        serve_token: "mock-serve-token".into(),
        reasonix: Default::default(),
    };
    Arc::new(GwState::from_config(&cfg).expect("build state"))
}

fn test_jwt() -> String {
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

/// mock serve：/submit 记录收到的 body(input 字段)；/status 兜底(非 submit 对照用)。
fn mock_serve_submit(recorded: Arc<tokio::sync::Mutex<Option<String>>>) -> Router {
    Router::new()
        .route(
            "/submit",
            any(move |req: axum::extract::Request| {
                let recorded = recorded.clone();
                async move {
                    let bytes = to_bytes(req.into_body(), 1024 * 1024)
                        .await
                        .unwrap_or_default();
                    let v: serde_json::Value =
                        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                    *recorded.lock().await = v
                        .get("input")
                        .and_then(|i| i.as_str())
                        .map(|s| s.to_string());
                    "ok"
                }
            }),
        )
        .route("/status", any(move || async move { "ok" }))
}

/// 核心：submit 的 input 被注入工具提示前缀（原文本保留在后）。
#[tokio::test]
async fn submit_input_prefixed_with_tool_hint() {
    let recorded = Arc::new(tokio::sync::Mutex::new(None::<String>));
    let mock = spawn_axum(mock_serve_submit(recorded.clone())).await;
    let gw = spawn_axum(build_router(test_state(mock))).await;

    let resp = reqwest::Client::new()
        .post(format!("{gw}/api/agent/submit"))
        .bearer_auth(test_jwt())
        .json(&serde_json::json!({ "input": "devA 的温度是多少" }))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);

    let got = recorded
        .lock()
        .await
        .clone()
        .expect("mock must record input");
    // 提示前缀在，原输入保留在后（不被吞掉）。
    assert!(
        got.contains("rsiot"),
        "hint must mention rsiot MCP tools; got {got}"
    );
    assert!(
        got.ends_with("devA 的温度是多少"),
        "original input preserved at end; got {got}"
    );
    assert!(
        got.len() > "devA 的温度是多少".len(),
        "hint actually prepended; got {got}"
    );
}

/// 对照：非 submit 路径（/api/agent/status）不注入、body 不碰。
#[tokio::test]
async fn other_agent_paths_not_injected() {
    let recorded = Arc::new(tokio::sync::Mutex::new(None::<String>));
    let mock = spawn_axum(mock_serve_submit(recorded.clone())).await;
    let gw = spawn_axum(build_router(test_state(mock))).await;

    // 打一个非 submit 的 agent 路径（GET /api/agent/status 走 forward 透传）。
    let resp = reqwest::Client::new()
        .get(format!("{gw}/api/agent/status"))
        .bearer_auth(test_jwt())
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    // mock 只记录 submit 的 body——非 submit 请求不该留下任何 input 记录。
    assert!(
        recorded.lock().await.is_none(),
        "non-submit path must not be injected"
    );
}

/// 对照：submit body 无 input 字段（畸形）→ 原样透传不 panic。
#[tokio::test]
async fn submit_without_input_passes_through() {
    let recorded = Arc::new(tokio::sync::Mutex::new(None::<String>));
    let mock = spawn_axum(mock_serve_submit(recorded.clone())).await;
    let gw = spawn_axum(build_router(test_state(mock))).await;

    let resp = reqwest::Client::new()
        .post(format!("{gw}/api/agent/submit"))
        .bearer_auth(test_jwt())
        .body("not-json")
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 200, "malformed body must pass through");
}

/// 超限（>1MB）body → 413 快速失败（审查 finding：stale Content-Length 透传会让
/// 上游收截断 body 报 502；to_bytes 已消费原 body 无法透传，413 是唯一诚实路径）。
#[tokio::test]
async fn submit_over_1mb_returns_413() {
    let recorded = Arc::new(tokio::sync::Mutex::new(None::<String>));
    let mock = spawn_axum(mock_serve_submit(recorded.clone())).await;
    let gw = spawn_axum(build_router(test_state(mock))).await;

    let big = "x".repeat(1024 * 1024 + 1000); // 1MB + 1KB
    let resp = reqwest::Client::new()
        .post(format!("{gw}/api/agent/submit"))
        .bearer_auth(test_jwt())
        .body(big)
        .send()
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "over-1MB submit must fail fast with 413, not 502"
    );
    // 上游不得收到任何请求（截断/空 body 都不该到达）。
    assert!(
        recorded.lock().await.is_none(),
        "upstream must not receive oversized body"
    );
}
