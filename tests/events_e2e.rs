//! SSE 实时推送端到端（阶段一）：日志 + 报文 + 实例状态。
//!
//! - 纯函数单测：`parse_types` 解析 `?types=` 过滤。
//! - HTTP 集成测：JWT 保护 + SSE 帧格式（`event:`/`data:`）+ 事件总线扇出。
//!
//! 注意：SSE 推送需真 EventBus（非 dummy）——test_app 建真 channel，共享给
//! HostContext（emit_state）/ TraceRegistry（dump）/ AppState（SSE 订阅）。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::event_bus::{EventBus, EventPayload};
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::log_ringbuffer::LogRingBuffer;
use rs_iot::plugin::instance::{InstState, InstanceManager};
use rs_iot::types::{HostKind, Hwid};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 建测试 app + 真 EventBus（三方共享：HostContext/AppState/TraceRegistry）。
async fn test_app() -> (axum::Router, lux::ServerHandle, EventBus) {
    let data_dir = common::test_data_dir("sse");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let metrics = Arc::new(rs_iot::process_metrics::ProcessMetrics::new());
    let event_bus = EventBus::new(256);
    // TraceRegistry 用真 bus：dump 激活时扇出 Packet。
    let trace_registry = rs_iot::io_trace::TraceRegistry::with_sender(event_bus.clone());
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown,
        ctl_registry: CtlRegistry::new(metrics.clone()),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: metrics.clone(),
        trace_registry: trace_registry.clone(),
        event_tx: event_bus.clone(),
    };
    let (hub_tx, _hub_rx) = tokio::sync::mpsc::channel(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));
    let state = AppState {
        lux: client.clone(),
        mgr,
        hwid: Hwid::new("it-hwid"),
        jwt_secret: "test-secret-0123456789abcdef".into(),
        version: "itest",
        metrics: metrics.clone(),
        trace_registry,
        log_ringbuffer: Arc::new(LogRingBuffer::new(2000)),
        event_bus: event_bus.clone(),
        config: std::sync::Arc::new(rs_iot::config::AppConfig::default()),
    };
    (build_router(state), handle, event_bus)
}

/// 登录拿 JWT（admin/admin）。
async fn jwt(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/user/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"user_id":"admin","pass":"admin"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["token"].as_str().unwrap().to_string()
}

/// 登录拿 Set-Cookie（EventSource 同源带 cookie，测试模拟此路径）。
async fn login_cookie(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/user/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"user_id":"admin","pass":"admin"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    // 取 "token=xxx; ..." 的 "token=xxx" 段。
    cookie.split(';').next().unwrap().to_string()
}

// ---------- 纯函数单测：parse_types ----------

#[test]
fn parse_types_empty_returns_none() {
    use rs_iot::api::routes_events::parse_types;
    assert!(parse_types("").is_none(), "空 → None（全推）");
    assert!(parse_types("   ").is_none(), "纯空白 → None");
}

#[test]
fn parse_types_single() {
    use rs_iot::api::routes_events::parse_types;
    let s = parse_types("log").unwrap();
    assert_eq!(s, ["log".into()].into_iter().collect());
}

#[test]
fn parse_types_multiple_trims_whitespace() {
    use rs_iot::api::routes_events::parse_types;
    let s = parse_types(" log , packet , state ").unwrap();
    assert_eq!(s.len(), 3);
    assert!(s.contains("log") && s.contains("packet") && s.contains("state"));
}

#[test]
fn parse_types_all_empty_entries_returns_none() {
    use rs_iot::api::routes_events::parse_types;
    // ",," 全空条目 → None（全推）
    assert!(parse_types(",,").is_none());
}

// ---------- HTTP 集成测 ----------

#[tokio::test]
async fn events_without_jwt_401() {
    // SSE 端点默认 JWT 保护（cookie 或 Authorization）。
    let (app, handle, _bus) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn events_with_jwt_streams_state_event() {
    // 带 JWT 建立 SSE 连接 → publish State → 响应体含 `event: state` 帧。
    // SSE 是长连接流（KeepAlive 周期发注释帧，流不结束），用 into_data_stream 逐 chunk
    // 读，检到目标帧即断（不等流结束）。
    use futures_util::StreamExt;
    let (app, handle, bus) = test_app().await;
    let tok = jwt(&app).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/events?types=state")
                .header(header::AUTHORIZATION, tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );

    // 连接已建立（handler 已 subscribe）。publish 一条 State。
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    bus.publish(EventPayload::State {
        inst_id: "mb-tcp-2002".into(),
        state: InstState::Running,
        app_code: "modbusClient".into(),
        host_kind: HostKind::Builtin,
        ts: "2026-08-11T00:00:00Z".into(),
    });

    // 逐 chunk 读，检到 state 帧即成功（3s 超时兜底，防卡死）。
    let mut stream = resp.into_body().into_data_stream();
    let mut buf = String::new();
    let found = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            buf.push_str(&String::from_utf8_lossy(&chunk));
            if buf.contains("event: state") && buf.contains("mb-tcp-2002") {
                return true;
            }
        }
        false
    })
    .await;
    assert!(found.unwrap_or(false), "应含 state 事件帧 + inst_id: {buf}");
    assert!(buf.contains("modbusClient"), "data 应含 app_code: {buf}");
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn events_with_cookie_auth_ok() {
    // EventSource 同源带 cookie：验 cookie 鉴权路径通（与 JWT 等价）。
    let (app, handle, _bus) = test_app().await;
    let cookie = login_cookie(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/events")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn events_filter_excludes_other_kinds() {
    // ?types=log → publish Packet 不应出现在响应（被过滤）。
    use futures_util::StreamExt;
    let (app, handle, bus) = test_app().await;
    let tok = jwt(&app).await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/events?types=log")
                .header(header::AUTHORIZATION, tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    bus.publish(EventPayload::Packet {
        inst_id: "x".into(),
        dir: "tx",
        hex: "01 02".into(),
        ts: "2026-08-11T00:00:00Z".into(),
    });

    // 读 1s：packet 帧应被过滤（不出现）；keep-alive 注释帧可能出现。
    let mut stream = resp.into_body().into_data_stream();
    let mut buf = String::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
    })
    .await;
    assert!(
        !buf.contains("event: packet"),
        "types=log 应过滤掉 packet: {buf}"
    );
    common::stop_lux(handle).await;
}
