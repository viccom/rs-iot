//! startinst 唯一启动守卫 e2e（2026-08-23 计划）：对已运行实例二次启动必须被拒
//! （管理器 Err + REST 409），且不产生"幽灵任务"（hub 采样速率保持单份）；
//! stop 后重启放行（ed76dc2 语义保留）。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::test_data_dir;
use rs_iot::error::RsIotError;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{AppType, HostKind, Hwid, InstConfig, PluginName, Sample};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// harness：lux + HostContext + InstanceManager（hub_rx 留给采样计数）。
async fn harness(
    tag: &str,
) -> (
    Arc<InstanceManager>,
    tokio::sync::mpsc::Receiver<Sample>,
    CancellationToken,
    lux::ServerHandle,
    lux::EmbeddedClient,
) {
    let data_dir = test_data_dir(tag);
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");
    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(Arc::new(rs_iot::process_metrics::ProcessMetrics::new())),
        bridge_registry: BridgeRegistry::new(),
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
        history: None,
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(256);
    (
        Arc::new(InstanceManager::new(ctx, hub_tx)),
        hub_rx,
        shutdown,
        handle,
        client,
    )
}

/// dataSim 实例（period 1s、单设备单标签——采样速率可控可断言）。
/// M-2：先注册隐式模型+devreg 设备，返回绑定形态实例。
async fn sim_inst(inst_id: &str, client: &lux::EmbeddedClient) -> InstConfig {
    let bindings = common::register_devices_as_models(
        client,
        inst_id,
        "dataSim",
        vec![rs_iot::types::DeviceSpec {
            label: "d".into(),
            value: "devGuard".into(),
            children: vec![],
            slave_id: None,
        }],
    )
    .await;
    InstConfig {
        inst_id: inst_id.into(),
        inst_name: "guard".into(),
        app_code: PluginName::new("dataSim"),
        app_type: AppType::South,
        auto_start: false,
        app_config: json!({ "period": 1 }),
        devices: bindings,
        host_kind: HostKind::Builtin,
    }
}

/// 收 hub 采样 `secs` 秒返回条数（排空后计时窗内新增）。
async fn count_samples(rx: &mut tokio::sync::mpsc::Receiver<Sample>, secs: f64) -> usize {
    while rx.try_recv().is_ok() {} // 排空历史
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(secs);
    let mut n = 0;
    while tokio::time::timeout_at(deadline, rx.recv()).await.is_ok() {
        n += 1;
    }
    n
}

#[tokio::test]
async fn double_start_rejected_no_ghost_and_restartable() {
    let (mgr, mut hub_rx, shutdown, handle, client) = harness("guard-mgr").await;

    // 首次启动 Ok
    mgr.start_source(sim_inst("g1", &client).await)
        .await
        .expect("首次启动");

    // 二次启动必须被拒（当前 bug：返回 Ok 且产生幽灵任务——此断言先红）
    let err = mgr
        .start_source(sim_inst("g1", &client).await)
        .await
        .expect_err("对运行中实例二次启动应被拒");
    assert!(
        matches!(err, RsIotError::InstanceAlreadyRunning(ref id) if id == "g1"),
        "应 InstanceAlreadyRunning，得到 {err:?}"
    );

    // 幽灵任务回归锁：period=1s 单任务 4.5s 应 2..=6 条；双任务会 ~7-10
    tokio::time::sleep(Duration::from_millis(1500)).await; // 等 phase 首拍过窗
    let n = count_samples(&mut hub_rx, 4.5).await;
    assert!(
        (2..=6).contains(&n),
        "单任务采样速率 2..=6，得到 {n}（>6 提示幽灵任务双写）"
    );

    // stop 后重启放行（ed76dc2「stop 后可重启」语义）
    mgr.stop("g1").await.expect("stop");
    mgr.start_source(sim_inst("g1", &client).await)
        .await
        .expect("stop 后重启应放行");

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn rest_startinst_double_returns_409() {
    use rs_iot::api::{build_router, AppState};
    use tower::ServiceExt;

    let data_dir = test_data_dir("guard-rest");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");
    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(Arc::new(rs_iot::process_metrics::ProcessMetrics::new())),
        bridge_registry: BridgeRegistry::new(),
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
        history: None,
    };
    let (hub_tx, _hub_rx) = tokio::sync::mpsc::channel::<Sample>(256);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));
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
        history_stats: None,
        history_engine_state: Arc::from("disabled"),
        history_reader: None,
    };
    let app = build_router(state);

    // 登录
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
    let token = serde_json::from_slice::<serde_json::Value>(&bytes).expect("json")["token"]
        .as_str()
        .expect("token")
        .to_string();

    // 持久化实例配置（startinst 从 lux instances 读）
    let cfg = sim_inst("g2", &client).await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/inst/set")
                .header(header::AUTHORIZATION, &token)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "inst_id": "g2", "object": serde_json::to_value(&cfg).expect("cfg") })
                        .to_string(),
                ))
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::OK, "inst/set 应成功");

    // 第一次 startinst → 200
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/startinst")
                .header(header::AUTHORIZATION, &token)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"inst_id":"g2"}"#))
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::OK, "首次 startinst 应 200");

    // 第二次 startinst → 409（当前 bug 返回 200——先红）
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/startinst")
                .header(header::AUTHORIZATION, &token)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"inst_id":"g2"}"#))
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::CONFLICT, "二次 startinst 应 409");

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
}
