//! /metrics 端到端:axum oneshot,验证免 JWT + 200 + text/plain + 进程级 metric。

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
    let data_dir = common::test_data_dir("metrics");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown,
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
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
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        log_ringbuffer: std::sync::Arc::new(rs_iot::log_ringbuffer::LogRingBuffer::new(2000)),
        event_bus: rs_iot::event_bus::EventBus::dummy(),
        config: std::sync::Arc::new(rs_iot::config::AppConfig::default()),
    };
    (build_router(state), handle)
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

#[tokio::test]
async fn metrics_is_public_returns_text() {
    let (app, handle) = test_app().await;

    // 不带 JWT(Prometheus scrape 场景)→ 应 200(非 401)
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK, "免 JWT 应放行");
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .expect("content-type")
        .to_str()
        .expect("utf8")
        .to_string();
    assert!(
        ct.contains("text/plain"),
        "content-type 应含 text/plain,实际 {ct}"
    );
    let body = body_text(resp).await;
    // 进程级 metric 齐全
    assert!(body.contains("rs_iot_info{hwid=\"it-hwid\""), "含进程标识");
    assert!(body.contains("# TYPE rs_iot_ctl_send_total counter"));
    assert!(body.contains("rs_iot_uptime_seconds"));
    assert!(body.contains("rs_iot_process_cpu_pct"));
    assert!(body.contains("rs_iot_mqtt_connected"));
    // per-instance family 头存在(无实例时只有头,无行)
    assert!(body.contains("# TYPE rs_iot_instance_info gauge"));

    common::stop_lux(handle).await;
}

#[tokio::test]
async fn metrics_returns_ok_with_no_instances() {
    // 无实例注册时 /metrics 仍应 200 + 非空 body(进程级指标独立于实例存在)。
    // 注:真实 stat 读错误路径(实例启动但未 flush stat)未覆盖,留待 per-instance e2e。
    let (app, handle) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(!body.is_empty(), "body 非空");

    common::stop_lux(handle).await;
}
