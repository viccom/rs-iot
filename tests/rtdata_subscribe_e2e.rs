//! 实时数据订阅端到端：OPC UA 式语义（snapshot 全量 → 周期 datachange 只推变化）。
//!
//! 覆盖：JWT 保护、snapshot 全量+过滤、值变才推（值同不推）、设备删除 removed。
//! 测试基建对标 tests/events_e2e.rs（同 test_app 骨架）；SSE 读流用窗口收集断言
//! （先收 snapshot 窗口，再变更数据，再收 datachange 窗口）。

mod common;

use axum::body::{Body, BodyDataStream};
use axum::http::{header, Request, StatusCode};
use futures_util::StreamExt;
use rs_iot::api::{build_router, AppState};
use rs_iot::event_bus::EventBus;
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::log_ringbuffer::LogRingBuffer;
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{Hwid, Sample, SampleValue};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 建测试 app（订阅端点读 lux，不依赖 EventBus 事件流；骨架同 events_e2e 便于对照）。
async fn test_app(name: &str) -> (axum::Router, lux::ServerHandle, lux::EmbeddedClient) {
    let data_dir = common::test_data_dir(name);
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
        history: None,
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
        history_stats: None,
        history_engine_state: std::sync::Arc::from("disabled"),
        history_reader: None,
    };
    (build_router(state), handle, client)
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

/// 建订阅连接（带 JWT）：断言 200 + text/event-stream，返回 body 流。
async fn connect(app: &axum::Router, query: &str, tok: &str) -> BodyDataStream {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/rtdatas/subscribe{query}"))
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
    resp.into_body().into_data_stream()
}

/// 收集窗口：读流 dur 时长（或流尽）→ String（窗口式断言基建，events_e2e 内联模式的函数化）。
async fn collect_window(stream: &mut BodyDataStream, dur: std::time::Duration) -> String {
    let mut buf = String::new();
    let _ = tokio::time::timeout(dur, async {
        while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
    })
    .await;
    buf
}

/// 种子数据：write_batch 一条 Sample（field 形如 <devid>.<tag>，cell[2] 为值串）。
async fn seed(lux: &lux::EmbeddedClient, devid: &str, pts: &[(&str, &str)], ts: i64) {
    let mut values = BTreeMap::new();
    for (t, v) in pts {
        values.insert((*t).to_string(), SampleValue::Str((*v).to_string()));
    }
    let s = Sample {
        hwid: Hwid::new("it-hwid"),
        devid: devid.into(),
        values,
        timestamp: ts,
    };
    rs_iot::db::rtdata::write_batch(lux, std::slice::from_ref(&s))
        .await
        .unwrap();
}

#[tokio::test]
async fn subscribe_without_jwt_401() {
    let (app, handle, _lux) = test_app("sub401").await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/rtdatas/subscribe")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn subscribe_with_jwt_200_event_stream() {
    let (app, handle, _lux) = test_app("sub200").await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "", &tok).await;
    // 骨架（空流）下流立即结束——窗口收集返回空或 keep-alive，不卡死即可。
    let buf = collect_window(&mut stream, std::time::Duration::from_millis(300)).await;
    assert!(buf.len() < 1 << 20);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn snapshot_pushes_all_points_on_connect() {
    let (app, handle, lux) = test_app("subsnap").await;
    seed(&lux, "devA", &[("t1", "1.5"), ("t2", "2")], 1_700_000_000).await;
    seed(&lux, "devB", &[("t1", "3")], 1_700_000_000).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?interval=200", &tok).await;
    let buf = collect_window(&mut stream, std::time::Duration::from_millis(800)).await;
    // 两设备各一帧 snapshot；cell 形如 [ts,"localTime","value"]（值串，与 /rtdatas 同构）。
    assert!(buf.contains("event: snapshot"), "应含 snapshot 帧: {buf}");
    assert!(
        buf.contains("\"dev\":\"devA\"") && buf.contains("\"dev\":\"devB\""),
        "逐设备分帧: {buf}"
    );
    assert!(buf.contains("devA.t1") && buf.contains("1.5"), "{buf}");
    assert!(buf.contains("devA.t2") && buf.contains("devB.t1"), "{buf}");
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn snapshot_respects_dev_and_tag_filter() {
    let (app, handle, lux) = test_app("subfilter").await;
    seed(&lux, "devA", &[("temp", "1"), ("hum", "2")], 1).await;
    seed(&lux, "devB", &[("temp", "3")], 1).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?dev_id=devA&tag=temp&interval=200", &tok).await;
    let buf = collect_window(&mut stream, std::time::Duration::from_millis(800)).await;
    assert!(buf.contains("devA.temp"), "{buf}");
    assert!(!buf.contains("devA.hum"), "tag 过滤应排除: {buf}");
    assert!(!buf.contains("devB"), "dev 过滤应排除 devB: {buf}");
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn datachange_only_pushes_changed_values() {
    let (app, handle, lux) = test_app("subdiff").await;
    seed(&lux, "devA", &[("t1", "1"), ("t2", "2")], 1_700_000_000).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?dev_id=devA&interval=100", &tok).await;
    // 窗口1：snapshot 收全。
    let buf1 = collect_window(&mut stream, std::time::Duration::from_millis(600)).await;
    assert!(
        buf1.contains("event: snapshot") && buf1.contains("devA.t1"),
        "{buf1}"
    );
    // 变更：t1 新值；t2 同值不同 ts（采集周期写回同值场景）。
    seed(&lux, "devA", &[("t1", "9"), ("t2", "2")], 1_700_000_100).await;
    let buf2 = collect_window(&mut stream, std::time::Duration::from_millis(1500)).await;
    assert!(
        buf2.contains("event: datachange"),
        "应含 datachange: {buf2}"
    );
    assert!(buf2.contains("devA.t1") && buf2.contains("9"), "{buf2}");
    assert!(!buf2.contains("devA.t2"), "值未变不应重推: {buf2}");
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn removed_on_device_delete() {
    let (app, handle, lux) = test_app("subdel").await;
    seed(&lux, "devA", &[("t1", "1")], 1).await;
    seed(&lux, "devB", &[("t1", "2")], 1).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?interval=100", &tok).await;
    let buf1 = collect_window(&mut stream, std::time::Duration::from_millis(600)).await;
    assert!(
        buf1.contains("devA.t1") && buf1.contains("devB.t1"),
        "{buf1}"
    );
    rs_iot::db::rtdata::delete_device(&lux, "devA")
        .await
        .unwrap();
    let buf2 = collect_window(&mut stream, std::time::Duration::from_millis(1500)).await;
    assert!(
        buf2.contains("event: datachange") && buf2.contains("removed"),
        "{buf2}"
    );
    assert!(buf2.contains("devA.t1"), "removed 应列 devA.t1: {buf2}");
    common::stop_lux(handle).await;
}
