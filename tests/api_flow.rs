//! REST API 集成测试：真实 lux + bootstrap + axum router（tower oneshot，
//! 不发真实网络请求）。覆盖 JWT 鉴权中间件：白名单放行 / token 访问 / 拒绝无 token。

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

/// 构造一个真实状态的 axum app（lux + bootstrap + mgr）。
async fn test_app() -> (axum::Router, lux::ServerHandle) {
    let data_dir = common::test_data_dir("api");
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
        history_stats: None,
        history_engine_state: std::sync::Arc::from("disabled"),
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

#[tokio::test]
async fn login_is_public_and_returns_token() {
    let (app, handle) = test_app().await;

    let resp = app
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
    let json = body_json(resp).await;
    assert_eq!(json["status"], "ok");
    assert!(
        json["token"].as_str().is_some_and(|t| !t.is_empty()),
        "login should return non-empty token"
    );

    common::stop_lux(handle).await;
}

#[tokio::test]
async fn protected_route_requires_token() {
    let (app, handle) = test_app().await;

    // 无 token → 401
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/sysinfo")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    common::stop_lux(handle).await;
}

#[tokio::test]
async fn token_grants_access_to_sysinfo() {
    let (app, handle) = test_app().await;

    // 先登录拿 token
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
    let token = body_json(resp).await["token"]
        .as_str()
        .expect("token")
        .to_string();

    // 带 token 访问受保护路由 → 200 + hwid
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/sysinfo")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["hwid"], "it-hwid");
    assert_eq!(json["version"], "itest");

    common::stop_lux(handle).await;
}

/// B1（审计：写路径谎报 ok）正向守门：set 正常路径 200 ok + 回读一致
/// ——锁定修复（失败路径返 5xx）不破坏正常路径。
/// 失败路径行为红测**不可达**（豁免登记）：EmbeddedClient 进程内直连
/// store，shutdown_and_wait 后 execute 仍成功，无注入口；serde_json::Value
/// 序列化恒成功。失败分支以代码审查 + 本守门护航。
#[tokio::test]
async fn user_set_normal_path_roundtrip() {
    let (app, handle) = test_app().await;

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
    let token = body_json(resp).await["token"]
        .as_str()
        .expect("token")
        .to_string();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/user/set")
                .header(header::AUTHORIZATION, &token)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"user_id":"u1","object":{"role":"viewer"}}"#))
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["status"], "ok");

    // 回读一致（落库真实生效）
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/user/get?user_id=u1")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["u1"]["role"], "viewer", "set 后回读必须一致: {json}");

    common::stop_lux(handle).await;
}
