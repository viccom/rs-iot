//! 设备模型注册表端到端（M-1 起累加；计划 §五 T4/T6）。
//!
//! T4 骨架：test_app（lux + InstanceManager + REST 路由，无 pipeline——模型面
//! 不经数据链路）+ JWT 登录冒烟。T6 起累加 REST 场景：401 保护 / devmodel
//! 建查删 / 重名 409 / devreg 引用完整性 4xx 矩阵 / effective 视图。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::event_bus::EventBus;
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::log_ringbuffer::LogRingBuffer;
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{Hwid, Sample};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 建测试 app：lux + InstanceManager + REST 路由（模型面无数据链路，不起
/// pipeline）。返回 (router, lux handle)。
async fn test_app(name: &str) -> (axum::Router, lux::ServerHandle) {
    let data_dir = common::test_data_dir(name);
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let metrics = Arc::new(rs_iot::process_metrics::ProcessMetrics::new());
    let event_bus = EventBus::new(256);
    let trace_registry = rs_iot::io_trace::TraceRegistry::with_sender(event_bus.clone());
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(metrics.clone()),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: metrics.clone(),
        trace_registry: trace_registry.clone(),
        event_tx: event_bus.clone(),
    };
    let (hub_tx, _hub_rx) = tokio::sync::mpsc::channel::<Sample>(16);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));

    let state = AppState {
        lux: client.clone(),
        mgr,
        hwid: Hwid::new("it-hwid"),
        jwt_secret: "test-secret-0123456789abcdef".into(),
        version: "itest",
        metrics: metrics.clone(),
        trace_registry,
        log_ringbuffer: Arc::new(LogRingBuffer::new(64)),
        event_bus,
        config: Arc::new(rs_iot::config::AppConfig::default()),
    };
    (build_router(state), handle)
}

/// 登录拿 JWT（admin/admin，同 tag_registry_e2e 口径）。
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
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["token"].as_str().unwrap().to_string()
}

/// 带认证 JSON POST（helper：T6 场景复用）。
#[allow(dead_code)]
async fn post_json(
    app: &axum::Router,
    token: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, v)
}

/// 带认证 GET（helper：T6 场景复用）。
#[allow(dead_code)]
async fn get_json(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, v)
}

/// 骨架冒烟：登录链路可用（模型 REST 场景的前置依赖）。
#[tokio::test]
async fn skeleton_login_smoke() {
    let (app, _handle) = test_app("model-skeleton").await;
    let token = jwt(&app).await;
    assert!(!token.is_empty());
}

use tower::ServiceExt;
