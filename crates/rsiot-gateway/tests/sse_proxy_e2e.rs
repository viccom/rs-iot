//! B1-1 SSE 裸字节流透传端到端验证（决定性测试）。
//!
//! 验证 gateway `forward()` 的核心假设（plan-gateway-tech-selection.md §四悬念2）：
//! **两条 SSE 流（rs-iot 有 event: 头 / serve 无 event: 头裸 NDJSON）经 gateway 裸透传后，
//! 帧格式不被破坏**。若 serve 风格流被注入 `data:` 前缀或帧边界错乱，则 forward 实现有问题，
//! 需回 §四悬念1 复核。
//!
//! 架构：mock 双上游（axum :0 OS 分配端口）+ gateway（build_router :0）+ reqwest 真实连接。
//! 不用 tower oneshot——SSE 流式 + 跨包切分必须真实网络栈。

use std::sync::Arc;

use axum::body::Body;
use axum::response::{sse::Event, IntoResponse, Response, Sse};
use axum::routing::any;
use axum::Router;
use futures_util::stream;
use jsonwebtoken::{encode, EncodingKey, Header};
use rsiot_gateway::auth::Claims;
use rsiot_gateway::{build_router, GwConfig, GwState};
use tokio::net::TcpListener;

/// 起一个 axum 服务在 :0，返回 (实际监听地址, shutdown 句柄)。
///
/// 测试结束 drop join_handle 即停服务（axum::serve 的 future 被 drop 中止）。
async fn spawn_axum(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://{addr}");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve mock");
    });
    url
}

/// 构造测试用 GwState（手动指定 base 指向 mock，绕过 from_config 读文件）。
fn test_state(rsiot_base: String, serve_base: String) -> Arc<GwState> {
    let cfg = GwConfig {
        listen: "127.0.0.1:0".into(),
        rsiot_base,
        serve_base,
        jwt_secret: "test-secret".into(),
        serve_token: "mock-serve-token".into(),
        reasonix: Default::default(),
    };
    Arc::new(GwState::from_config(&cfg).expect("build state"))
}

/// 签发测试 JWT（与 gateway auth.rs Claims 同结构，共享 test-secret）。
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

/// mock rs-iot SSE handler：用 axum Sse+Event 发**有 event: 头**帧（对标 rs-iot
/// routes_events.rs:61-68 `Event::default().event(kind).json_data(payload)`）。
///
/// 发 3 帧 event: log，每帧 data 是 JSON。
async fn rsiot_style_sse() -> Response {
    let frames = vec![
        Ok::<_, std::convert::Infallible>(
            Event::default()
                .event("log")
                .json_data(serde_json::json!({"msg": "frame1"}))
                .expect("json_data"),
        ),
        Ok(Event::default()
            .event("log")
            .json_data(serde_json::json!({"msg": "frame2"}))
            .expect("json_data")),
        Ok(Event::default()
            .event("log")
            .json_data(serde_json::json!({"msg": "frame3"}))
            .expect("json_data")),
    ];
    Sse::new(stream::iter(frames)).into_response()
}

/// mock reasonix serve SSE handler：用裸 `Body::from_stream` 发**无 event: 头**的
/// 裸 NDJSON 帧（对标真实 serve `eventwire/wire.go:16`——kind 在 JSON body 内，
/// 不写 event: 头）。这是决定性测试的关键对照。
///
/// 故意用小 chunk（每帧独立 yield）模拟真实 TCP 分包。
async fn serve_style_naked_ndjson() -> Response {
    let chunks: Vec<Result<_, std::io::Error>> = vec![
        // 模拟 serve 事件流：裸 JSON + \n\n，无 data: 前缀、无 event: 头。
        Ok(bytes::Bytes::from_static(
            b"{\"type\":\"text\",\"text\":\"hello\"}\n\n",
        )),
        Ok(bytes::Bytes::from_static(
            b"{\"type\":\"tool_call\",\"name\":\"rsiot_list_devices\"}\n\n",
        )),
        Ok(bytes::Bytes::from_static(b": ping\n\n")),
    ];
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream::iter(chunks)))
        .expect("build response")
}

/// 测试 1：rs-iot 风格 SSE（有 event: 头）经 gateway 透传，帧格式保留。
///
/// 验证意图（规则7）：前端 EventSource 靠 `event:` 头分发，gateway 必须原样保留。
/// 收到的字节应含 `event: log` + `data:`（rs-iot 风格本就有 data:，透传不破坏）。
#[tokio::test]
async fn rsiot_sse_event_header_preserved() {
    let mock = spawn_axum(Router::new().route("/api/v1/events", any(rsiot_style_sse))).await;
    let gw = spawn_axum(build_router(test_state(mock.clone(), mock.clone()))).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{gw}/api/rsiot/api/v1/events"))
        .bearer_auth(test_jwt())
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );

    let body = resp.text().await.expect("read body");
    // 3 帧都应保留 event: log + data:（rs-iot 风格特征）。
    assert_eq!(
        body.matches("event: log").count(),
        3,
        "event header preserved"
    );
    assert_eq!(body.matches("frame1").count(), 1);
    assert_eq!(body.matches("frame3").count(), 1);
}

/// 测试 2（决定性）：serve 风格无 event: 头裸 NDJSON 经 gateway 透传，**不注入 data: 前缀**。
///
/// 验证意图（规则7 + 报告 §四悬念1）：reasonix 前端按裸 JSON 流解析（`JSON.parse(line)`），
/// 若 gateway 用 axum Sse/Event 包装会注入 `data:` 前缀破坏解析。裸透传必须保持
/// `{...}\n\n` 原样。这是 forward() 实现正确性的核心判据。
#[tokio::test]
async fn serve_naked_ndjson_not_wrapped_with_data_prefix() {
    let mock = spawn_axum(Router::new().route("/events", any(serve_style_naked_ndjson))).await;
    let gw = spawn_axum(build_router(test_state(mock.clone(), mock.clone()))).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{gw}/api/agent/events"))
        .bearer_auth(test_jwt())
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("read body");

    // 决定性断言：裸 JSON 行原样存在，**未被注入 data: 前缀**。
    assert!(
        body.contains("{\"type\":\"text\",\"text\":\"hello\"}\n\n"),
        "naked NDJSON preserved without data: prefix; got:\n{body}"
    );
    assert!(
        body.contains("{\"type\":\"tool_call\""),
        "second frame preserved; got:\n{body}"
    );
    // 注释帧 : ping 也应原样透传（serve 心跳）。
    assert!(body.contains(": ping\n\n"), "ping comment preserved");

    // 反向断言：不应出现 axum Event 包装的 data: 前缀（若出现说明 forward 错用了 Sse）。
    assert!(
        !body.contains("data: {\"type\""),
        "forward must NOT inject data: prefix for serve naked NDJSON; got:\n{body}"
    );
}

/// 测试 3：多帧 + 跨 chunk 边界，gateway 不破坏 `\n\n` 帧分隔。
///
/// 验证意图（报告 §四悬念2）：gateway 是字节管道不做帧边界状态机，前端 EventSource
/// 自解 `\n\n`。多帧串行透传后，按 `\n\n` split 应得预期的非空帧数。
#[tokio::test]
async fn multi_frame_boundaries_intact() {
    let mock = spawn_axum(Router::new().route("/events", any(serve_style_naked_ndjson))).await;
    let gw = spawn_axum(build_router(test_state(mock.clone(), mock.clone()))).await;
    let client = reqwest::Client::new();

    let body = client
        .get(format!("{gw}/api/agent/events"))
        .bearer_auth(test_jwt())
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");

    // 按 \n\n 切分，去空串 + 去注释帧（: ping），剩 2 个 JSON 帧。
    let json_frames: Vec<&str> = body
        .split("\n\n")
        .filter(|s| !s.is_empty() && !s.starts_with(':'))
        .collect();
    assert_eq!(
        json_frames.len(),
        2,
        "exactly 2 JSON frames after split; got {body}"
    );
    // 每帧都能 JSON.parse（决定性：前端裸 JSON 解析可行）。
    for f in &json_frames {
        serde_json::from_str::<serde_json::Value>(f).expect("frame parses as JSON");
    }
}

/// 测试 4（B1-2 顺带）：层3 serve cookie 注入。
///
/// 验证意图（报告 §四层3 + §七 B1-2）：gateway 转发 serve 时注入
/// `Cookie: reasonix_token=<token>`，serve 只认 cookie 不认 Bearer。
/// mock serve 校验收到的 cookie，错则 401。
#[tokio::test]
async fn serve_cookie_injected_on_forward() {
    // 用 Mutex 收集 mock serve 收到的 Cookie 头。
    let received_cookie = Arc::new(tokio::sync::Mutex::new(None::<String>));
    let captured = received_cookie.clone();

    let mock_serve = spawn_axum(Router::new().route(
        "/status",
        any(move |req: axum::extract::Request| {
            let captured = captured.clone();
            async move {
                let ck = req
                    .headers()
                    .get(axum::http::header::COOKIE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                *captured.lock().await = ck;
                "ok"
            }
        }),
    ))
    .await;
    let gw = spawn_axum(build_router(test_state(mock_serve.clone(), mock_serve))).await;

    // 经 gateway 调 serve，客户端只带 JWT（不带 serve cookie）。
    reqwest::Client::new()
        .get(format!("{gw}/api/agent/status"))
        .bearer_auth(test_jwt())
        .send()
        .await
        .expect("request");

    // mock serve 应收到 gateway 注入的 Cookie: reasonix_token=mock-serve-token。
    let got = received_cookie.lock().await.clone();
    assert_eq!(
        got.as_deref(),
        Some("reasonix_token=mock-serve-token"),
        "gateway must inject serve cookie"
    );
}
