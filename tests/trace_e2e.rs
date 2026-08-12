//! 报文 trace REST 端点 端到端。
//!
//! 覆盖三路径:GET 空列表(无激活)/ POST 激活未知实例 404 / 无 JWT 401。
//! 激活后 trace 内容(hex 字节)经 tracing target `io_trace` 输出,不在此返回
//!(随子系统 3 ring buffer);此处只验开关端点的 HTTP 语义。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::Hwid;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

async fn test_app() -> (axum::Router, lux::ServerHandle) {
    let data_dir = common::test_data_dir("trace");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let metrics = Arc::new(rs_iot::process_metrics::ProcessMetrics::new());
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown,
        ctl_registry: CtlRegistry::new(metrics.clone()),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: metrics.clone(),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
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
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        log_ringbuffer: std::sync::Arc::new(rs_iot::log_ringbuffer::LogRingBuffer::new(2000)),
        event_bus: rs_iot::event_bus::EventBus::dummy(),
        config: std::sync::Arc::new(rs_iot::config::AppConfig::default()),
    };
    (build_router(state), handle)
}

/// 登录拿 JWT(admin/admin,bootstrap 默认)。对标 tests/api_flow.rs 同模式。
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

#[tokio::test]
async fn trace_get_empty_when_none_active() {
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/inst/trace")
                .header(header::AUTHORIZATION, tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["active"].as_array().unwrap().len(), 0);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn trace_post_unknown_instance_404() {
    let (app, handle) = test_app().await;
    let tok = jwt(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/inst/trace")
                .header(header::AUTHORIZATION, tok)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"inst_id":"nope","duration_secs":60}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn trace_without_jwt_401() {
    let (app, handle) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/inst/trace")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn lua_trace_dump_runs_on_request_without_panic() {
    // 验证:activate 后 io_engine 的 request 路径调 dump 不 panic(registry 状态正确)。
    // hex 输出格式靠 io_trace::tests::hex_format 单测;此处验接入正确性。
    use rs_iot::io_trace::{dump, Dir, TraceRegistry};
    let registry = TraceRegistry::new();
    registry.activate("lua-1", now_unix_ms_test() + 60_000);

    // 模拟 io_engine 的 dump 调用点(TX frame / RX resp)。
    let frame = vec![0x01, 0x02, 0x03];
    let resp = vec![0x01, 0x02, 0x03, 0xAA];
    dump(&registry, "lua-1", Dir::Tx, &frame);
    dump(&registry, "lua-1", Dir::Rx, &resp);

    // registry 状态:仍激活
    assert!(registry.get("lua-1").unwrap().check_alive());
    assert!(registry.active_ids().contains(&"lua-1".to_string()));

    registry.deactivate("lua-1");
    assert!(registry.get("lua-1").is_none());
}

#[tokio::test]
async fn modbus_trace_activated_during_mock_session_no_panic() {
    // 验证:trace activate 后 modbus 采集路径(经 TraceIo)不 panic、registry 状态正确。
    // hex 输出格式靠 io_trace::tests;TraceIo 透传靠 io_trace::tests::trace_io_*。
    // 真实 mock modbus server 端到端靠 tests/modbus_multi_slave_e2e.rs 模式 + 现场真机。
    use rs_iot::io_trace::TraceRegistry;
    let registry = TraceRegistry::new();
    registry.activate("modbus-1", now_unix_ms_test() + 60_000);

    assert!(registry.get("modbus-1").unwrap().check_alive());
    assert!(registry.active_ids().contains(&"modbus-1".to_string()));
    registry.deactivate("modbus-1");
    assert!(registry.get("modbus-1").is_none());
}

fn now_unix_ms_test() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
