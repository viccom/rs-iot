//! 历史可见性薄切片 e2e（Phase 3 前置）：/sysstatus history 块 + /metrics
//! `rs_iot_history_*_total` 7 family。AppState 注入测试态（stats 预置非零值 /
//! engine_state），断言两出口的精确输出与省略语义（引擎非 ok 不输出）。

#![cfg(feature = "history")]

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::history::rows::HistoryStats;
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::Hwid;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 预置非零七计数器（每计数器取互异值，精确断言防串位）。
fn preset_stats() -> Arc<HistoryStats> {
    let s = HistoryStats::default();
    s.sent.store(11, Ordering::Relaxed);
    s.written.store(7, Ordering::Relaxed);
    s.dropped.store(3, Ordering::Relaxed);
    s.deadbanded.store(5, Ordering::Relaxed);
    s.skipped.store(2, Ordering::Relaxed);
    s.lane_mismatch.store(1, Ordering::Relaxed);
    s.errors.store(9, Ordering::Relaxed);
    Arc::new(s)
}

/// 构造注入指定 history 测试态的 axum app（其余照 metrics_e2e 范本）。
async fn test_app(
    history_stats: Option<Arc<HistoryStats>>,
    history_engine_state: Arc<str>,
) -> (axum::Router, lux::ServerHandle) {
    let data_dir = common::test_data_dir("hist_vis");
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
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        log_ringbuffer: std::sync::Arc::new(rs_iot::log_ringbuffer::LogRingBuffer::new(2000)),
        event_bus: rs_iot::event_bus::EventBus::dummy(),
        config: std::sync::Arc::new(rs_iot::config::AppConfig::default()),
        history_stats,
        history_engine_state,
        history_reader: None,
        history_telemetry: None,
        rtdata_watch: std::sync::Arc::new(rs_iot::api::rtdata_watch::RtdataWatchService::new()),
    };
    (build_router(state), handle)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("json body")
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

/// 登录拿 JWT（sysstatus 受保护，照 api_flow 范本）。
async fn login_token(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/user/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"user_id":"admin","pass":"admin"}"#))
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["token"]
        .as_str()
        .expect("token")
        .to_string()
}

/// /sysstatus：Some(stats) + engine="ok" → history 块 feature/enabled/engine
/// 与七计数器值精确断言（配置默认 enabled=false 与句柄在并存——注入态语义：
/// 配置关但句柄在时 stats 仍真实）。
#[tokio::test]
async fn sysstatus_history_block_ok() {
    let (app, handle) = test_app(Some(preset_stats()), Arc::from("ok")).await;
    let token = login_token(&app).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/sysstatus")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let hist = &json["history"];
    assert_eq!(hist["feature"], true, "测试带 history feature 编译,恒 true");
    assert_eq!(hist["enabled"], false, "默认配置 enabled=false");
    assert_eq!(hist["engine"], "ok");
    let stats = &hist["stats"];
    assert!(
        stats.is_object(),
        "engine==ok 时 stats 非 null,实际: {stats}"
    );
    assert_eq!(stats["sent"], 11);
    assert_eq!(stats["written"], 7);
    assert_eq!(stats["dropped"], 3);
    assert_eq!(stats["deadbanded"], 5);
    assert_eq!(stats["skipped"], 2);
    assert_eq!(stats["lane_mismatch"], 1);
    assert_eq!(stats["errors"], 9);

    common::stop_lux(handle).await;
}

/// /sysstatus：stats=None + engine="disabled" → feature 仍 true（测试带 feature
/// 编译）、stats 恒 null（仅 engine=="ok" 时非 null）。
#[tokio::test]
async fn sysstatus_history_block_absent() {
    let (app, handle) = test_app(None, Arc::from("disabled")).await;
    let token = login_token(&app).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/sysstatus")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let hist = &json["history"];
    assert_eq!(hist["feature"], true);
    assert_eq!(hist["engine"], "disabled");
    assert!(
        hist["stats"].is_null(),
        "engine 非 ok 时 stats 恒 null,实际: {}",
        hist["stats"]
    );

    common::stop_lux(handle).await;
}

/// /metrics：Some(stats) → 7 个 `rs_iot_history_*_total` family 头 + 正确数值；
/// None 态 → 全部 7 个不出现（引擎非 ok 不输出，避免误导零值）。
#[tokio::test]
async fn metrics_history_families() {
    let families: [(&str, &str); 7] = [
        ("rs_iot_history_sent_total", "11"),
        ("rs_iot_history_written_total", "7"),
        ("rs_iot_history_dropped_total", "3"),
        ("rs_iot_history_deadbanded_total", "5"),
        ("rs_iot_history_skipped_total", "2"),
        ("rs_iot_history_lane_mismatch_total", "1"),
        ("rs_iot_history_errors_total", "9"),
    ];

    // Some(stats) 态：7 family 头 + 数值行齐全
    let (app, handle) = test_app(Some(preset_stats()), Arc::from("ok")).await;
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
    assert_eq!(resp.status(), StatusCode::OK, "/metrics 免 JWT");
    let body = body_text(resp).await;
    for (name, val) in families {
        assert!(
            body.contains(&format!("# TYPE {name} counter")),
            "{name} family 头缺失:\n{body}"
        );
        assert!(
            body.contains(&format!("{name} {val}")),
            "{name} 数值行缺失(期望 {val}):\n{body}"
        );
    }
    common::stop_lux(handle).await;

    // None 态：7 family 全部不出现
    let (app, handle) = test_app(None, Arc::from("disabled")).await;
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
    for (name, _) in families {
        assert!(
            !body.contains(name),
            "{name} 不应出现(引擎非 ok,防误导零值):\n{body}"
        );
    }
    common::stop_lux(handle).await;
}
