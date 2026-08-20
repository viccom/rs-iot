//! R4 核查（阶段 B 计划 Task 1）：北向实例（`app_type=North`）启动接线负向测试。
//!
//! 当前 registry 无任何已注册 Sink 插件（`SinkEntry` 空——luaNorth 是 Task 6
//! 首个接线者），故本文件只覆盖「北向实例经管理面启动 → 找不到 sink 插件 →
//! 返回 `Config` 错误、不 panic、运行态无残留」这一负向层；正向 Running /
//! stop / Failed 生命周期断言留给 Task 6（luaNorth 注册后在此文件追加）。
//!
//! 两条启动路径的覆盖：
//! - REST `POST /api/v1/startinst`：本文件真实断言——修复前对 North 返回
//!   400 "only south..."，修复后分发到 `start_sink` → 无插件时 500 Config 错误。
//! - lux 自动启动（`lib.rs::try_start_from_lux`）：North 分发逻辑与 REST 同构
//!   （South→start_source / North→start_sink）；无注册 sink 时其外部行为与
//!   "静默跳过"不可区分（均不计数、无状态残留），无法写行为区分断言，
//!   由 Task 6 正向 e2e 兜底。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 北向实例配置：app_code 用 `luaNorth`（Task 6 注册后的目标插件名；
/// 当前未注册，`start_sink` 应在 `find_sink` 即失败）。
fn north_inst(id: &str) -> InstConfig {
    InstConfig {
        inst_id: id.into(),
        inst_name: id.into(),
        app_code: PluginName::new("luaNorth"),
        app_type: AppType::North,
        auto_start: true,
        app_config: serde_json::json!({}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: id.into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    }
}

/// 构造测试 HostContext（照 `instance_failed_e2e.rs` 模式）。
fn test_ctx(client: lux::EmbeddedClient) -> HostContext {
    HostContext {
        lux: client,
        hwid: Hwid::new("it-hwid"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(Arc::new(rs_iot::process_metrics::ProcessMetrics::new())),
        bridge_registry: BridgeRegistry::new(),
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    }
}

/// 真实 axum app（lux + bootstrap + mgr，照 `api_flow.rs` 模式），附 mgr 供运行态断言。
async fn test_app() -> (axum::Router, Arc<InstanceManager>, lux::ServerHandle) {
    let data_dir = common::test_data_dir("lua_north_t1");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(test_ctx(client.clone()), hub_tx));
    let state = AppState {
        lux: client.clone(),
        mgr: mgr.clone(),
        hwid: Hwid::new("it-hwid"),
        jwt_secret: "test-secret-0123456789abcdef".into(),
        version: "itest",
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        log_ringbuffer: Arc::new(rs_iot::log_ringbuffer::LogRingBuffer::new(2000)),
        event_bus: rs_iot::event_bus::EventBus::dummy(),
        config: Arc::new(rs_iot::config::AppConfig::default()),
    };
    (build_router(state), mgr, handle)
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    String::from_utf8_lossy(&bytes).to_string()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("json body")
}

/// login 拿 JWT（startinst 不在白名单）。
async fn login_token(app: axum::Router) -> String {
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
    body_json(resp).await["token"]
        .as_str()
        .expect("token")
        .to_string()
}

/// InstanceManager 层负向：North 实例 `start_sink` 在无注册 sink 插件时返回
/// `Config` 错误（Task 6 前唯一可达层），且不 panic、运行态无残留。
#[tokio::test]
async fn north_start_sink_without_plugin_returns_config_error() {
    let data_dir = common::test_data_dir("lua_north_sink_err");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(16);
    let mgr = InstanceManager::new(test_ctx(client), hub_tx);

    let err = mgr
        .start_sink(north_inst("north-err"))
        .await
        .expect_err("无注册 sink 插件时 start_sink 必须报错");
    match err {
        rs_iot::error::RsIotError::Config(msg) => assert!(
            msg.contains("sink plugin not found: luaNorth"),
            "错误消息应指出插件缺失，实际: {msg}"
        ),
        other => panic!("应为 Config 错误，实际: {other:?}"),
    }
    // 失败发生在 find_sink（spawn 前），运行态不得残留。
    assert!(
        !mgr.status().contains_key("north-err"),
        "启动失败的实例不得残留运行态: {:?}",
        mgr.status()
    );

    common::stop_lux(handle).await;
}

/// REST 启动路径负向（R4 修复的回归锁）：North 实例经 `startinst` 启动不再被
/// 400 "only south" 拒绝，而是分发到 `start_sink` → 无插件时 500 Config 错误。
/// 顺带验证实例 CRUD（`inst/set`）对 North 配置的写入放行（R4 核查项）。
#[tokio::test]
async fn rest_startinst_north_dispatches_to_start_sink() {
    let (app, mgr, handle) = test_app().await;
    let token = login_token(app.clone()).await;

    // 经 REST 实例 CRUD 写入 North autoStart 实例配置（startinst 从 lux HGET 读回）。
    let inst = serde_json::to_value(north_inst("north-rest")).expect("serialize");
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/inst/set")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "inst_id": "north-rest",
                        "object": inst,
                    }))
                    .expect("json"),
                ))
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["status"], "ok", "CRUD set 应接受 North 配置");

    // 启动：修复前 400 "only south(source)..."；修复后走 start_sink → 无插件 500。
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/startinst")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"inst_id":"north-rest"}"#))
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "North 实例应分发到 start_sink（无插件 → Config 错误 500），而非 400 拒绝"
    );
    let body = body_text(resp).await;
    assert!(
        body.contains("sink plugin not found"),
        "错误应来自 find_sink 失败，实际: {body}"
    );
    // 启动失败不残留运行态、不 panic。
    assert!(
        !mgr.status().contains_key("north-rest"),
        "启动失败的实例不得残留运行态: {:?}",
        mgr.status()
    );

    common::stop_lux(handle).await;
}
