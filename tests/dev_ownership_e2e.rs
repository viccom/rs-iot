//! 设备归属闭环 e2e（D-OWN-2 T3）：启动预登记 + cascade_delete 归属反查清理
//! + purge 同步清归属（spec `2026-08-21-device-ownership-design.md` §2）。
//!
//! 全链路真实形态：lux + bootstrap + mgr 真启动 dataSim 实例 + HTTP oneshot 走
//! `/api/v1/inst/delete`、`/api/v1/purge` 生产端点（cascade_delete 是 pub(crate)，
//! 端点即其唯一生产入口）。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 构造真实 app + 可再操作的 mgr/lux（照 api_flow.rs test_app，多返回 mgr 与 client）。
async fn test_app(
    name: &str,
) -> (
    axum::Router,
    lux::ServerHandle,
    Arc<InstanceManager>,
    lux::EmbeddedClient,
) {
    let data_dir = common::test_data_dir(name);
    let _ = std::fs::remove_dir_all(&data_dir);
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("own-hwid"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(Arc::new(rs_iot::process_metrics::ProcessMetrics::new())),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    };
    let (hub_tx, _hub_rx) = tokio::sync::mpsc::channel(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));
    let state = AppState {
        lux: client.clone(),
        mgr: mgr.clone(),
        hwid: Hwid::new("own-hwid"),
        jwt_secret: "own-test-secret-0123456789".into(),
        version: "itest",
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        log_ringbuffer: Arc::new(rs_iot::log_ringbuffer::LogRingBuffer::new(2000)),
        event_bus: rs_iot::event_bus::EventBus::dummy(),
        config: Arc::new(rs_iot::config::AppConfig::default()),
    };
    (build_router(state), handle, mgr, client)
}

/// 登录 admin 拿 JWT（delete/purge 是受保护端点）。
async fn login_token(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/user/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"user_id":"admin","pass":"admin"}"#))
                .expect("login request"),
        )
        .await
        .expect("call login");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("login body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("login json");
    json["token"].as_str().expect("token").to_string()
}

/// POST JSON 受保护端点（附 token）。
async fn post_json(
    app: &axum::Router,
    token: &str,
    uri: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, token)
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("call app");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// dataSim 实例配置：period=3600 保证测试窗口内不出数（归属只能来自启动预登记，
/// 排除 SampleSender 出口登记的干扰——那正是本套件要与之区分的另一条登记路径）。
fn sim_inst(inst_id: &str, devices: &[&str]) -> InstConfig {
    InstConfig {
        inst_id: inst_id.into(),
        inst_name: inst_id.into(),
        app_code: PluginName::new("dataSim"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({"period": 3600}),
        devices: devices
            .iter()
            .map(|d| DeviceSpec {
                label: (*d).into(),
                value: (*d).into(),
                children: vec![],
                slave_id: None,
            })
            .collect(),
        host_kind: HostKind::Builtin,
    }
}

/// key 是否存在（EXISTS → bool）。
async fn key_exists(c: &lux::EmbeddedClient, key: &str) -> bool {
    let v = c.execute_value("EXISTS", &[key]).await.expect("EXISTS");
    !matches!(v, lux::EmbeddedValue::Int(0) | lux::EmbeddedValue::Nil)
}

/// dev2inst field 原始值（Nil → None）。
async fn dev2inst_raw(c: &lux::EmbeddedClient, devid: &str) -> Option<String> {
    let v = c
        .execute_value("HGET", &["dev2inst", devid])
        .await
        .expect("HGET dev2inst");
    match v {
        lux::EmbeddedValue::Bulk(b) if !b.is_empty() => {
            Some(String::from_utf8_lossy(&b).into_owned())
        }
        lux::EmbeddedValue::Simple(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// 启动预登记（T3 兜底，spec §2）：配置设备在首条数据前就入册——防"启动后
/// 未出数即删"漏归属。period=3600 下 SampleSender 出口登记不可能触发，断言
/// start_source 返回即归属在册（预登记在 start_source 内 await 完成）。
#[tokio::test]
async fn start_source_preregisters_config_devices() {
    let (_app, handle, mgr, lux) = test_app("own_preregister").await;

    mgr.start_source(sim_inst("own-a", &["own-dev1"]))
        .await
        .expect("start");

    assert_eq!(
        dev2inst_raw(&lux, "own-dev1").await.as_deref(),
        Some("own-a"),
        "start_source 应对配置 devices[].value 预登记 dev2inst（首条数据前入册）"
    );

    common::stop_lux(handle).await;
}

/// cascade_delete 归属闭环（D-OWN-2 核心）：
/// - 配置设备（预登记）own-dev1 与运行时设备 own-rt1（手动 note 模拟 lua/set_value
///   派生）均清 rtdata 且 dev2inst 无残留字段；
/// - 双宿主 own-dev2（本实例 + 其它实例共写）：删本实例**保留** rtdata（最后宿主
///   才清），dev2inst 剩其它宿主；
/// - 安全网：own-devU 归属记录被抹（模拟预登记失败/旧数据目录无 dev2inst）→
///   配置声明路径仍清其 rtdata（回归保障——归属化重构不得丢原配置清理语义）。
#[tokio::test]
async fn cascade_delete_closes_ownership_loop() {
    let (app, handle, mgr, lux) = test_app("own_cascade").await;
    let token = login_token(&app).await;

    // 1. 真启动实例（配置设备 own-dev1/own-devU 经预登记在册；period=3600 不出数）。
    mgr.start_source(sim_inst("own-a", &["own-dev1", "own-devU"]))
        .await
        .expect("start");
    // 安全网构造：抹掉 own-devU 的归属记录——模拟"归属表缺记录"（预登记失败/
    // 旧数据目录），此时配置声明是唯一清理依据。
    lux.execute("HDEL", &["dev2inst", "own-devU"])
        .await
        .expect("hdel");
    // 2. 运行时设备归属（模拟 lua set_value / transform 派生设备的 SampleSender 登记）。
    rs_iot::db::dev_registry::note(&lux, "own-rt1", "own-a")
        .await
        .expect("note rt1");
    // 3. 双宿主：own-dev2 同时归属本实例与其它实例 own-b。
    rs_iot::db::dev_registry::note(&lux, "own-dev2", "own-a")
        .await
        .expect("note dev2 a");
    rs_iot::db::dev_registry::note(&lux, "own-dev2", "own-b")
        .await
        .expect("note dev2 b");

    // 模拟各设备已有 rtdata。
    for dev in ["own-dev1", "own-devU", "own-rt1", "own-dev2"] {
        lux.execute("SET", &[&format!("rtdata:{dev}"), "1"])
            .await
            .expect("set rtdata");
    }

    // 删本实例（HTTP 生产路径 → cascade_delete）。
    let (status, body) = post_json(
        &app,
        &token,
        "/api/v1/inst/delete",
        r#"{"inst_id":"own-a"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "deleted");

    // 配置设备 + 运行时设备：最后宿主 → rtdata 清、归属字段删。
    assert!(
        !key_exists(&lux, "rtdata:own-dev1").await,
        "配置设备（最后宿主）rtdata 应清"
    );
    assert!(
        !key_exists(&lux, "rtdata:own-rt1").await,
        "运行时设备（最后宿主）rtdata 应清——归属反查闭环的核心"
    );
    assert_eq!(
        dev2inst_raw(&lux, "own-dev1").await,
        None,
        "own-dev1 归属字段应 HDEL"
    );
    assert_eq!(
        dev2inst_raw(&lux, "own-rt1").await,
        None,
        "own-rt1 归属字段应 HDEL"
    );
    // 双宿主：仍有 own-b 共写 → 数据保留，归属剩 own-b。
    assert!(
        key_exists(&lux, "rtdata:own-dev2").await,
        "双宿主设备删其一，rtdata 必须保留（删一宿主不清数据）"
    );
    assert_eq!(
        dev2inst_raw(&lux, "own-dev2").await.as_deref(),
        Some("own-b"),
        "own-dev2 归属应剩另一宿主 own-b"
    );
    // 安全网：归属记录缺失的配置设备仍清 rtdata（配置声明路径兜底）。
    assert!(
        !key_exists(&lux, "rtdata:own-devU").await,
        "配置回退安全网：无归属记录的配置设备 rtdata 仍应清"
    );
    // 自身状态清理（stop/remove_state 原有逻辑不动）。
    assert!(
        !mgr.status().contains_key("own-a"),
        "删除后运行态不得残留（remove_state）"
    );

    common::stop_lux(handle).await;
}

/// purge 同步清归属表（spec §2）：scope=rtdata / scope=all 都在清 rtdata 后
/// `DEL dev2inst`——rtdata 没了归属无意义。
#[tokio::test]
async fn purge_clears_dev2inst_both_scopes() {
    let (app, handle, _mgr, lux) = test_app("own_purge").await;
    let token = login_token(&app).await;

    // scope=rtdata：rtdata 缓存与归属表同清。
    rs_iot::db::dev_registry::note(&lux, "own-p1", "own-i1")
        .await
        .expect("note");
    lux.execute("SET", &["rtdata:own-p1", "1"])
        .await
        .expect("set");
    let (status, body) = post_json(&app, &token, "/api/v1/purge", r#"{"scope":"rtdata"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "purged");
    assert!(
        !key_exists(&lux, "rtdata:own-p1").await,
        "scope=rtdata 清 rtdata 缓存"
    );
    assert_eq!(
        dev2inst_raw(&lux, "own-p1").await,
        None,
        "scope=rtdata 应同步清归属表（rtdata 已清，归属无意义）"
    );

    // scope=all：归属表同样清空（含 cascade 无法覆盖的孤儿归属）。
    rs_iot::db::dev_registry::note(&lux, "own-p2", "own-i2")
        .await
        .expect("note");
    let (status, body) = post_json(&app, &token, "/api/v1/purge", r#"{"scope":"all"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "purged");
    assert_eq!(
        dev2inst_raw(&lux, "own-p2").await,
        None,
        "scope=all 应清归属表"
    );

    common::stop_lux(handle).await;
}
