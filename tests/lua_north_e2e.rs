//! luaNorth 端到端（阶段 B T6：Sink 首个接线者）。
//!
//! - T1 负向层保留（前提随 T6 更新）：`start_sink` 对**未注册** sink 插件返回
//!   `Config` 错误、不 panic、运行态无残留——T6 前 luaNorth 未注册时用
//!   `luaNorth` 本名即可触发，注册后改用不存在的 `noSuchSink`（锁的是同一条
//!   find_sink 失败路径）。
//! - T6 正向层：lux 写入 north 实例（`app_type=North`/`app_code=luaNorth`）→
//!   REST startinst（与 `try_start_from_lux` 同构的 lux 读取 + North→start_sink
//!   分发）→ Running → `/inst/stats` 有 stat → stopinst → Stopped。
//! - 能力包断言（§5.2 矩阵 mqtt-north 列）：脚本顶层 assert 探针——ctx 无
//!   `request`/`transport`（C1 裁剪）、有 `set_value`/`kv_get`/`secret_get`/
//!   `send_command`；`cfg` 全局表已注入（Q5）。探针失败 → 脚本加载失败 →
//!   实例不进 Running（下方生命周期断言兜红）。
//! - manifest 契约：`@role mqtt-north` 缺 `up()` → 启动拒载（warn + 不进
//!   Running；run 提前返回 → supervisor 标 Failed）。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::{InstState, InstanceManager};
use rs_iot::types::{AppType, DeviceBinding, HostKind, Hwid, InstConfig, PluginName, Sample};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 北向实例配置（T6 起 luaNorth 已注册：负向测试用不存在的 `noSuchSink` 锁
/// find_sink 失败路径）。
fn north_inst(id: &str, app_code: &str, app_config: serde_json::Value) -> InstConfig {
    InstConfig {
        inst_id: id.into(),
        inst_name: id.into(),
        app_code: PluginName::new(app_code),
        app_type: AppType::North,
        auto_start: true,
        app_config,
        devices: vec![DeviceBinding {
            devid: id.into(),
            bind: Default::default(),
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
        history: None,
    }
}

/// 真实 axum app（lux + bootstrap + mgr，照 `api_flow.rs` 模式），附 mgr 供运行态断言。
async fn test_app(tag: &str) -> (axum::Router, Arc<InstanceManager>, lux::ServerHandle) {
    let data_dir = common::test_data_dir(tag);
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
        history_stats: None,
        history_engine_state: Arc::from("disabled"),
        history_reader: None,
        history_telemetry: None,
        rtdata_watch: std::sync::Arc::new(rs_iot::api::rtdata_watch::RtdataWatchService::new()),
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

/// login 拿 JWT（startinst/inst/stats 不在白名单）。
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

/// 经 REST 写入实例配置（`inst/set`，startinst 从 lux HGET 读回——与
/// `try_start_from_lux` 同数据源同分发）。
async fn inst_set(app: axum::Router, token: &str, inst: &InstConfig) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/inst/set")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "inst_id": inst.inst_id.as_str(),
                        "object": serde_json::to_value(inst).expect("serialize"),
                    }))
                    .expect("json"),
                ))
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK, "CRUD set 应接受 North 配置");
    assert_eq!(body_json(resp).await["status"], "ok");
}

/// InstanceManager 层负向：North 实例 `start_sink` 对未注册 sink 插件返回
/// `Config` 错误，且不 panic、运行态无残留（T6 注册 luaNorth 后用不存在的
/// 插件名锁同一条 find_sink 失败路径）。
#[tokio::test]
async fn north_start_sink_without_plugin_returns_config_error() {
    let data_dir = common::test_data_dir("lua_north_sink_err");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(16);
    let mgr = InstanceManager::new(test_ctx(client), hub_tx);

    let err = mgr
        .start_sink(north_inst("north-err", "noSuchSink", serde_json::json!({})))
        .await
        .expect_err("未注册 sink 插件时 start_sink 必须报错");
    match err {
        rs_iot::error::RsIotError::Config(msg) => assert!(
            msg.contains("sink plugin not found: noSuchSink"),
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

/// REST 启动路径负向（R4 回归锁）：North 实例经 `startinst` 启动不被 400
/// "only south" 拒绝，而是分发到 `start_sink` → 未注册插件 500 Config 错误。
#[tokio::test]
async fn rest_startinst_north_dispatches_to_start_sink() {
    let (app, mgr, handle) = test_app("lua_north_t1").await;
    let token = login_token(app.clone()).await;

    inst_set(
        app.clone(),
        &token,
        &north_inst("north-rest", "noSuchSink", serde_json::json!({})),
    )
    .await;

    // 启动：R4 修复前 400 "only south(source)..."；修复后走 start_sink → 无插件 500。
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

/// mqtt-north 最小脚本（T6 骨架契约）：manifest 头 + auth/up 必需回调 +
/// 顶层能力探针（register 先于 load_main ——探针失败 = 加载失败 = 不进 Running）。
const NORTH_SCRIPT: &str = r#"
-- @role mqtt-north
-- 能力包探针（§5.2 矩阵 mqtt-north 列）：C1 不注入；C0/C2/C4/C6 与 send_command 在。
assert(ctx.request == nil, "north ctx 应无 request（C1 裁剪）")
assert(ctx.transport == nil, "north ctx 应无 transport（C1 裁剪）")
assert(type(ctx.set_value) == "function", "north ctx 应有 set_value")
assert(type(ctx.kv_get) == "function", "north ctx 应有 kv_get")
assert(type(ctx.secret_get) == "function", "north ctx 应有 secret_get")
assert(type(ctx.send_command) == "function", "north ctx 应有 send_command")
-- cfg 全局表（Q5 宿主注入）：有效配置可见（port/batch 缺省 1883/50，period 显式 1）。
assert(cfg ~= nil, "cfg 全局表应注入")
assert(cfg.host == "127.0.0.1", "cfg.host 应来自 app_config")
assert(cfg.port == 1883, "cfg.port 缺省 1883")
assert(cfg.batch_max_count == 50, "cfg.batch_max_count 缺省 50")
assert(cfg.batch_max_secs == 1, "cfg.batch_max_secs 缺省取 period（1）")
assert(cfg.client_id == "it-hwid-north-1", "cfg.client_id 缺省 hwid-<inst_id>")
function auth(c) return {} end
function up(batch) return {} end
"#;

/// 正向生命周期（T1 计划的正向断言，T6 luaNorth 注册后启用）：lux 写入 north
/// 实例 → startinst（lux 读取 + North→start_sink 分发）→ Running →
/// `/inst/stats` 有 stat（script_version 已埋）→ stopinst → Stopped（stat
/// status=stopped，退出路径 flush 已跑）。
#[tokio::test]
async fn north_lifecycle_running_stats_stop() {
    let (app, mgr, handle) = test_app("lua_north_t6").await;
    let token = login_token(app.clone()).await;

    // 临时脚本目录（mqtt-north 最小契约 + 能力探针）。
    let scripts = std::env::temp_dir().join(format!("lua-north-{}", std::process::id()));
    let script_dir = scripts.join("n1");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(script_dir.join("main.lua"), NORTH_SCRIPT).expect("write");

    let inst = north_inst(
        "north-1",
        "luaNorth",
        serde_json::json!({
            "script": "n1",
            "host": "127.0.0.1",
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
    );
    inst_set(app.clone(), &token, &inst).await;

    // 启动（North→start_sink 分发；脚本探针任一失败 → 不进 Running）。
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/startinst")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"inst_id":"north-1"}"#))
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK, "{}", body_text(resp).await);
    assert_eq!(
        mgr.status().get("north-1"),
        Some(&InstState::Running),
        "luaNorth 实例应 Running（含脚本能力探针全过）: {:?}",
        mgr.status()
    );

    // 等 stat flush（StatTracker 启动即首 flush + 周期 5s）。
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/inst/stats?inst_id=north-1")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    let stats = body_json(resp).await;
    assert_eq!(stats["state"], "running", "/inst/stats state 应 running");
    let health = &stats["health"];
    assert!(
        health.is_object(),
        "/inst/stats 应有 health（stat:<inst> 已写入）"
    );
    assert_eq!(health["status"], "running");
    // extra 是 serde(flatten)：script_version 在 health 顶层。
    let sv = health["script_version"].as_str().unwrap_or("");
    assert!(
        !sv.is_empty() && sv != "unknown",
        "script_version 已埋：{sv}"
    );

    // 停止 → Stopped；退出路径 tracker flush → stat status=stopped。
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/stopinst")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"inst_id":"north-1"}"#))
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        mgr.status().get("north-1"),
        Some(&InstState::Stopped),
        "stopinst 后应 Stopped: {:?}",
        mgr.status()
    );
    let stat = rs_iot::db::stat::read(&handle.client(), "north-1")
        .await
        .expect("read stat")
        .expect("stat 应在（退出 flush）");
    assert_eq!(stat.status, "stopped", "退出路径应 flush status=stopped");

    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

/// manifest 多角色契约拒载：`@role mqtt-north` 缺 `up()` → warn + run 提前返回
/// → supervisor 标 Failed（不进 Running）、stat 不写（tracker 未构造）。
#[tokio::test]
async fn north_manifest_missing_up_rejected() {
    let data_dir = common::test_data_dir("lua_north_reject");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(16);
    let mgr = InstanceManager::new(test_ctx(client.clone()), hub_tx);

    let scripts = std::env::temp_dir().join(format!("lua-north-rj-{}", std::process::id()));
    let script_dir = scripts.join("nbad");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        "-- @role mqtt-north\nfunction auth(cfg) return {} end\n",
    )
    .expect("write");

    let inst = north_inst(
        "north-bad",
        "luaNorth",
        serde_json::json!({
            "script": "nbad",
            "host": "127.0.0.1",
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
    );
    mgr.start_sink(inst)
        .await
        .expect("start_sink 本身成功（拒载在 task 内）");

    // run() 内拒载 → task 退出 → supervisor 标 Failed（轮询到稳定态，≤5s）。
    let mut final_state = None;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(s) = mgr.status().get("north-bad") {
            if *s == InstState::Failed {
                final_state = Some(*s);
                break;
            }
        }
    }
    assert_eq!(
        final_state,
        Some(InstState::Failed),
        "缺 up() 应拒载（warn + task 退出 → Failed，不进 Running）: {:?}",
        mgr.status()
    );
    // 拒载发生在 tracker 构造前 → stat 不得写入。
    let stat = rs_iot::db::stat::read(&client, "north-bad")
        .await
        .expect("read stat");
    assert!(stat.is_none(), "拒载实例不应写 stat: {stat:?}");

    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}
