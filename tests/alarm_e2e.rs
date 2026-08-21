//! 告警 REST + SSE 端到端（Q12 宿主面，spec §4）。
//!
//! - JWT 保护：无凭证 GET/DELETE → 401。
//! - `GET /api/v1/alarms`：空表 `[]`；fire 两键后形状 `{key, level, source, message,
//!   ts, count}` 且按 ts 倒序。
//! - `DELETE /api/v1/alarms?key=`：清除后清单空；SSE 收到 `event: alarm` 的
//!   `action=clear` 帧（level/message 沿用清除前条目）；不存在 key 幂等 200 且不发事件。
//!
//! fire 无 REST 端点（spec §4 只有查询/清除；触发是 T4 脚本面 `ctx:fire_event`），
//! 测试经 `db::alarms::fire` 直写 lux 模拟宿主写入路径（与脚本面同函数，形状不漂移）。
//! SSE 断言照 tests/events_e2e.rs 既有模式（oneshot 建流 + into_data_stream 轮询）。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::db::alarms;
use rs_iot::event_bus::EventBus;
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::log_ringbuffer::LogRingBuffer;
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::Hwid;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 唯一 data_dir（pid + 纳秒）：alarms Hash 跨 lux 重启持久（spec §1.2，恰是被测
/// 特性），固定 tag 会把上次运行的残留告警带进本次「空清单」断言；唯一化顺带隔离
/// 同进程并行测试互不共享 lux 实例。
fn unique_data_dir() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    common::test_data_dir(&format!("alarm-{}-{nanos}", std::process::id()))
}

/// 建测试 app + 真 EventBus（AppState/SSE 共享同一 bus，照 events_e2e 模式）。
///
/// 返回的 lux client 与 AppState 持同一实例——测试经 `db::alarms::fire` 直写即宿主
/// 写入路径（router 已 with_state 拿不回，外面自持一份）。
async fn test_app() -> (axum::Router, lux::EmbeddedClient, lux::ServerHandle) {
    let data_dir = unique_data_dir();
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let metrics = Arc::new(rs_iot::process_metrics::ProcessMetrics::new());
    let event_bus = EventBus::new(256);
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
        event_bus,
        config: std::sync::Arc::new(rs_iot::config::AppConfig::default()),
    };
    (build_router(state), client, handle)
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

/// GET /api/v1/alarms（带 JWT）→ 状态码 + JSON body。
async fn get_list(app: &axum::Router, tok: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/alarms")
                .header(header::AUTHORIZATION, tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let code = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

// ---------- JWT 保护（spec §4：照 routes_secret 默认 JWT）----------

#[tokio::test]
async fn alarms_without_jwt_401() {
    let (app, _lux, handle) = test_app().await;
    for (method, uri) in [
        ("GET", "/api/v1/alarms"),
        ("DELETE", "/api/v1/alarms?key=k"),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
    }
    common::stop_lux(handle).await;
}

// ---------- GET 清单 ----------

#[tokio::test]
async fn alarms_list_empty_returns_empty_array() {
    let (app, _lux, handle) = test_app().await;
    let tok = jwt(&app).await;
    let (code, body) = get_list(&app, &tok).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, serde_json::json!([]), "空表应返回空数组");
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn alarms_list_shape_and_ts_desc() {
    let (app, lux, handle) = test_app().await;
    let tok = jwt(&app).await;

    // 直写宿主路径（fire 无 REST 端点，脚本面是 T4）：两键不同 ts，old 重复 fire 一次。
    alarms::fire(&lux, "old.key", "info", "inst-a", "旧告警", "2026-08-21T01:00:00+00:00")
        .await
        .unwrap();
    alarms::fire(&lux, "old.key", "warn", "inst-a", "旧告警升级", "2026-08-21T01:05:00+00:00")
        .await
        .unwrap();
    alarms::fire(&lux, "new.key", "error", "inst-b", "新告警", "2026-08-21T02:00:00+00:00")
        .await
        .unwrap();

    let (code, body) = get_list(&app, &tok).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!([
            {
                "key": "new.key", "level": "error", "source": "inst-b",
                "message": "新告警", "ts": "2026-08-21T02:00:00+00:00", "count": 1
            },
            {
                "key": "old.key", "level": "warn", "source": "inst-a",
                "message": "旧告警升级", "ts": "2026-08-21T01:05:00+00:00", "count": 2
            },
        ]),
        "ts 倒序 + 覆盖写取最新 level/message/ts + count 累计"
    );
    common::stop_lux(handle).await;
}

// ---------- DELETE 清除 + SSE Clear 事件 ----------

#[tokio::test]
async fn alarm_delete_clears_list_and_publishes_sse_clear() {
    use futures_util::StreamExt;
    let (app, lux, handle) = test_app().await;
    let tok = jwt(&app).await;

    alarms::fire(&lux, "dev1.temp_high", "warn", "inst-a", "温度过高", "2026-08-21T03:00:00+00:00")
        .await
        .unwrap();

    // 先建 SSE 连接（?types=alarm），handler 已 subscribe 后再 DELETE（resp 单独
    // 持有——不被后续 DELETE 响应遮蔽，流从本响应体读）。
    let sse = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/events?types=alarm")
                .header(header::AUTHORIZATION, &tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sse.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut stream = sse.into_body().into_data_stream();
    let mut buf = String::new();

    // 幂等前置：不存在 key 的 DELETE → 200；读窗 300ms 内无 alarm 帧（无事件）。
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/alarms?key=missing")
                .header(header::AUTHORIZATION, &tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "不存在 key 幂等 200");
    let (code, body) = get_list(&app, &tok).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body.as_array().map(|a| a.len()), Some(1), "幂等 DELETE 不动清单");
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), async {
        while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
    })
    .await;
    assert!(
        !buf.contains("event: alarm"),
        "不存在 key 的 DELETE 不应发事件: {buf}"
    );

    // 清除存在的 key：HDEL + publish Clear（level/message 沿用清除前条目）。
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/alarms?key=dev1.temp_high")
                .header(header::AUTHORIZATION, &tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // SSE 帧到达：event: alarm + action=clear + key + 沿用条目的 level/message。
    let found = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if buf.contains("event: alarm") && buf.contains("\"action\":\"clear\"") {
                return true;
            }
        }
        false
    })
    .await;
    assert!(
        found.unwrap_or(false),
        "应含 alarm clear 事件帧: {buf}"
    );
    assert!(buf.contains("dev1.temp_high"), "帧应含被清 key: {buf}");
    assert!(buf.contains("\"level\":\"warn\""), "level 沿用清除前条目: {buf}");
    assert!(buf.contains("温度过高"), "message 沿用清除前条目: {buf}");

    // 清除后清单空。
    let (code, body) = get_list(&app, &tok).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, serde_json::json!([]));
    common::stop_lux(handle).await;
}
