//! M7 Lua REST 端点集成测：stats 读 stat / reload 404 / scripts 列表形状 / JWT 保护。
//! spec §13 + §4。仿 api_flow.rs（router oneshot + JWT token）。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::db::stat::InstanceStat;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::Hwid;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 构造带（可选）预写 stat 的 app。
async fn app_with_stat(stat_inst: &str) -> (axum::Router, lux::ServerHandle) {
    let data_dir = common::test_data_dir("lua-api");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");
    if !stat_inst.is_empty() {
        // vm_restart_count / script_version 走 extra（M4 InstanceStat flatten 透传）。
        let mut extra = serde_json::Map::new();
        extra.insert("vm_restart_count".into(), serde_json::json!(1));
        extra.insert("script_version".into(), serde_json::json!("deadbeef"));
        let stat = InstanceStat {
            status: "running".into(),
            tick_count: 3,
            fail_count: 0,
            last_activity_ts: 99,
            last_activity_duration_ms: 5,
            last_err: String::new(),
            extra,
        };
        rs_iot::db::stat::write(&client, stat_inst, &stat)
            .await
            .expect("write stat");
    }
    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown,
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: BridgeRegistry::new(),
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

async fn login_token(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/user/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"user_id":"admin","pass":"admin"}"#))
                .expect("req"),
        )
        .await
        .expect("call");
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("body");
    serde_json::from_slice::<serde_json::Value>(&bytes).expect("json")["token"]
        .as_str()
        .expect("token")
        .to_string()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

#[tokio::test]
async fn stats_returns_written_stat() {
    let (app, handle) = app_with_stat("lua-api").await;
    let token = login_token(&app).await;
    // 实例指标已统一到 /api/v1/inst/stats（M4）；health 字段承载 InstanceStat。
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/inst/stats?inst_id=lua-api")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["health"]["status"], "running");
    assert_eq!(json["health"]["tick_count"], 3);
    assert_eq!(json["health"]["script_version"], "deadbeef");
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn stats_missing_health_is_null() {
    let (app, handle) = app_with_stat("").await;
    let token = login_token(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/inst/stats?inst_id=nonexist")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    // 统一端点恒返 {inst_id, app_code, host_kind, state, health}；无 stat 时 health=null。
    assert!(
        json["health"].is_null(),
        "无 stat 时 health 应为 null，得到 {json}"
    );
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn stats_requires_token() {
    let (app, handle) = app_with_stat("").await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/inst/stats?inst_id=x")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn reload_unknown_instance_404() {
    let (app, handle) = app_with_stat("").await;
    let token = login_token(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lua/reload?inst_id=nonexist")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn scripts_returns_list_shape() {
    let (app, handle) = app_with_stat("").await;
    let token = login_token(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/lua/scripts")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["scripts"].is_array(), "返回 {{ scripts: [...] }}");
    common::stop_lux(handle).await;
}
