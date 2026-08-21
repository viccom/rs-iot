//! luaTransform 端到端（阶段 C T7：Source 订阅型薄壳，Q19 三薄插件之三）。
//!
//! - 正向生命周期：lux 写 transform 实例（`app_type=South`/`app_code=luaTransform`）
//!   → REST startinst（South→start_source 分发）→ Running → on_timer 真跑（kv
//!   计数 ≥1）→ `/inst/stats` 有 stat → stopinst → Stopped。
//! - 能力包断言（§5.2 矩阵 transform 列，T4 e2e 面）：脚本顶层 assert 探针——
//!   ctx 无 `request`/`transport`/`send_command`/`http_get`（C1/C3/C5 裁剪）、
//!   有 `set_value`/`kv_get`/`kv_set`/`secret_get`（C0/C2/C4）。探针失败 → 脚本
//!   加载失败 → 实例不进 Running（下方生命周期断言兜红；照 luaNorth e2e 探针模式）。
//! - 订阅路径：真实 EventBus 发布 Rtdata 事件 → 宿主模式过滤（"dev1.temp*"/"dev2"）
//!   → `on_rtdata(devid,tag,value)` 逐点回调（kv 落痕）+ 非命中点/非订阅 dev 不投递
//!   + 非 Rtdata 事件（startinst 的 State 事件同总线流经）被忽略。
//! - manifest 契约：`@role transform` 缺 `on_timer`/`on_rtdata` → 启动拒载（warn +
//!   不进 Running；run 提前返回 → supervisor 标 Failed，T5 契约的 e2e 面）。
//! - 自环防护（T8 硬性验收，Q6）：真实链路（hub → pipeline 50ms 攒批 → publish
//!   Rtdata 回 EventBus）——实例订阅自身会写的 tag，`set_value` 产出回流不触发自身
//!   `on_rtdata`（kv 计数稳定）；跨实例不误伤（A 写 → B 订阅收到，防护 per-instance）。
//! - agg-filter 示例实测（T9 硬性验收——「一个清洗/聚合脚本上线」）：读
//!   `examples/lua-scripts/agg-filter/main.lua` 文件本体（非内嵌副本，防示例与实现
//!   漂移），真实链路跑通野点剔除 + 滑动窗口均值（kv cfg_window 可配窗口大小）。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::event_bus::{EventBus, EventPayload, RtdataSample};
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::{InstState, InstanceManager};
use rs_iot::plugin::traits::Processor;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{
    AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample, SampleValue,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 南向（Source）transform 实例配置。
fn transform_inst(id: &str, app_code: &str, app_config: serde_json::Value) -> InstConfig {
    InstConfig {
        inst_id: id.into(),
        inst_name: id.into(),
        app_code: PluginName::new(app_code),
        app_type: AppType::South,
        auto_start: true,
        app_config,
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: id.into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    }
}

/// 构造测试 HostContext（照 `lua_north_e2e.rs` 模式；event_bus 参数化——订阅路径
/// 用真实总线，生命周期测试用 dummy）。
fn test_ctx(client: lux::EmbeddedClient, event_bus: EventBus) -> HostContext {
    HostContext {
        lux: client,
        hwid: Hwid::new("it-hwid"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(Arc::new(rs_iot::process_metrics::ProcessMetrics::new())),
        bridge_registry: BridgeRegistry::new(),
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: event_bus,
    }
}

/// 真实 axum app（lux + bootstrap + mgr，照 `lua_north_e2e.rs` 模式），附 mgr 供
/// 运行态断言。event_bus 同时进 HostContext（transform 订阅面）与 AppState（SSE）。
async fn test_app(
    tag: &str,
    event_bus: EventBus,
) -> (axum::Router, Arc<InstanceManager>, lux::ServerHandle) {
    let data_dir = common::test_data_dir(tag);
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(
        test_ctx(client.clone(), event_bus.clone()),
        hub_tx,
    ));
    let state = AppState {
        lux: client.clone(),
        mgr: mgr.clone(),
        hwid: Hwid::new("it-hwid"),
        jwt_secret: "test-secret-0123456789abcdef".into(),
        version: "itest",
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        log_ringbuffer: Arc::new(rs_iot::log_ringbuffer::LogRingBuffer::new(2000)),
        event_bus,
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

/// 经 REST 写入实例配置（`inst/set`，startinst 从 lux HGET 读回——与生产同数据源）。
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
    assert_eq!(resp.status(), StatusCode::OK, "CRUD set 应接受 South 配置");
    assert_eq!(body_json(resp).await["status"], "ok");
}

/// REST startinst / stopinst。
async fn start_inst(app: axum::Router, token: &str, inst_id: &str) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method("POST")
            .uri("/api/v1/startinst")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(r#"{{"inst_id":"{inst_id}"}}"#)))
            .expect("request"),
    )
    .await
    .expect("call app")
}

async fn stop_inst(app: axum::Router, token: &str, inst_id: &str) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/stopinst")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"inst_id":"{inst_id}"}}"#)))
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
}

/// 读脚本侧 kv（`ctx:kv_set` 落 lux `luakv:<inst>`；JSON 字符串带引号剥掉）。
async fn kv_str(client: &lux::EmbeddedClient, inst_id: &str, key: &str) -> Option<String> {
    rs_iot::db::luakv::get(client, inst_id, key)
        .await
        .expect("luakv get")
        .map(|s| s.trim_matches('"').to_string())
}

/// transform 最小脚本：manifest 头（on_timer/on_rtdata any-of 取 on_timer 满足）+
/// 顶层能力探针（§5.2 矩阵 transform 列：C1/C3/C5 ✗、C0/C2/C4 ✓）+ on_timer kv
/// 计数（生命周期测试断言 ≥1，证明 on_timer 真跑）。
const TIMER_SCRIPT: &str = r#"
-- @role transform
-- 能力包探针（§5.2 矩阵 transform 列）：C1/C3/C5 不注入；C0/C2/C4 在。
assert(ctx.request == nil, "transform ctx 应无 request（C1 裁剪）")
assert(ctx.transport == nil, "transform ctx 应无 transport（C1 裁剪）")
assert(ctx.send_command == nil, "transform ctx 应无 send_command（C3 裁剪）")
assert(ctx.http_get == nil, "transform ctx 应无 http_get（C5 裁剪）")
assert(type(ctx.set_value) == "function", "transform ctx 应有 set_value")
assert(type(ctx.kv_get) == "function", "transform ctx 应有 kv_get")
assert(type(ctx.kv_set) == "function", "transform ctx 应有 kv_set")
assert(type(ctx.secret_get) == "function", "transform ctx 应有 secret_get")
function on_timer()
  local n = (ctx:kv_get("ticks") or 0) + 1
  ctx:kv_set("ticks", n)
end
"#;

/// 正向生命周期（T7）：lux 写实例 → startinst（South→start_source 分发）→ Running
///（含能力探针全过）→ on_timer 周期回调真跑（kv ticks ≥1）→ `/inst/stats` 有
/// stat（script_version 已埋）→ stopinst → Stopped（退出 flush status=stopped）。
#[tokio::test]
async fn transform_lifecycle_running_stats_stop() {
    let (app, mgr, handle) = test_app("lua_transform_t7", EventBus::dummy()).await;
    let token = login_token(app.clone()).await;

    // 临时脚本目录（transform 最小契约 + 能力探针 + on_timer 计数）。
    let scripts = std::env::temp_dir().join(format!("lua-transform-{}", std::process::id()));
    let script_dir = scripts.join("t1");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(script_dir.join("main.lua"), TIMER_SCRIPT).expect("write");

    let inst = transform_inst(
        "transform-1",
        "luaTransform",
        serde_json::json!({
            "script": "t1",
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
    );
    inst_set(app.clone(), &token, &inst).await;

    // 启动（South→start_source 分发；脚本探针/dry-run 任一失败 → 不进 Running）。
    let resp = start_inst(app.clone(), &token, "transform-1").await;
    assert_eq!(resp.status(), StatusCode::OK, "{}", body_text(resp).await);
    // 留 300ms 让潜在拒载（探针失败/dry-run 失败）先翻 Failed，再锁 Running。
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        mgr.status().get("transform-1"),
        Some(&InstState::Running),
        "luaTransform 实例应 Running（含能力探针 + manifest 契约全过）: {:?}",
        mgr.status()
    );

    // on_timer 真跑：period=1s（首 tick 带 jitter 相位 <1s），轮询 kv ticks ≥1。
    let mut ticks: i64 = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        if let Some(s) = kv_str(&handle.client(), "transform-1", "ticks").await {
            if let Ok(n) = s.parse::<i64>() {
                ticks = n;
                if n >= 1 {
                    break;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    assert!(ticks >= 1, "on_timer 应周期执行（kv ticks 实测 {ticks}）");

    // /inst/stats：state=running + health（stat:<inst> 已写）+ script_version 已埋。
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/inst/stats?inst_id=transform-1")
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
    assert!(health.is_object(), "/inst/stats 应有 health");
    assert_eq!(health["status"], "running");
    let sv = health["script_version"].as_str().unwrap_or("");
    assert!(
        !sv.is_empty() && sv != "unknown",
        "script_version 已埋：{sv}"
    );

    // 停止 → Stopped；退出路径 tracker flush → stat status=stopped。
    stop_inst(app.clone(), &token, "transform-1").await;
    assert_eq!(
        mgr.status().get("transform-1"),
        Some(&InstState::Stopped),
        "stopinst 后应 Stopped: {:?}",
        mgr.status()
    );
    let stat = rs_iot::db::stat::read(&handle.client(), "transform-1")
        .await
        .expect("read stat")
        .expect("stat 应在（退出 flush）");
    assert_eq!(stat.status, "stopped", "退出路径应 flush status=stopped");

    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

/// 组 Rtdata 事件样本（dev → [(tag, value)]）。
fn rtdata_sample(devid: &str, vals: &[(&str, SampleValue)]) -> RtdataSample {
    RtdataSample {
        devid: devid.into(),
        values: vals
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
        ts: 1_700_000_000,
    }
}

/// 订阅路径（Q6 宿主过滤 + on_rtdata 逐点回调）：真实 EventBus 发布 Rtdata →
/// `subscribe=["dev1.temp*","dev2"]` 命中点回调（kv 落痕）——dev1.temp ✓、dev2.p ✓；
/// dev1.hum（tag glob 不中）/ dev3.x（dev 不订阅）不投递。非 Rtdata 事件（startinst
/// 的 State 事件同总线流经）被忽略（实例存活即证明）。
#[tokio::test]
async fn transform_on_rtdata_matching_and_filtered() {
    let event_bus = EventBus::new(64);
    let (app, mgr, handle) = test_app("lua_transform_rtdata", event_bus.clone()).await;
    let token = login_token(app.clone()).await;

    let scripts = std::env::temp_dir().join(format!("lua-transform-rt-{}", std::process::id()));
    let script_dir = scripts.join("t2");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        r#"-- @role transform
function on_rtdata(devid, tag, value)
  ctx:kv_set("got_" .. devid .. "_" .. tag, tostring(value))
end
"#,
    )
    .expect("write");

    let inst = transform_inst(
        "transform-2",
        "luaTransform",
        serde_json::json!({
            "script": "t2",
            "period": 5,
            "subscribe": ["dev1.temp*", "dev2"],
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
    );
    inst_set(app.clone(), &token, &inst).await;
    let resp = start_inst(app.clone(), &token, "transform-2").await;
    assert_eq!(resp.status(), StatusCode::OK, "{}", body_text(resp).await);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        mgr.status().get("transform-2"),
        Some(&InstState::Running),
        "实例应 Running: {:?}",
        mgr.status()
    );

    // 重发 + 轮询（订阅在 run() 内建立——与 startinst 返回有微小竞态，发布重试兜住；
    // broadcast 不回放，晚订阅的发布自然丢，重发无害）。
    let mut got_temp = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    while std::time::Instant::now() < deadline {
        event_bus.publish(EventPayload::Rtdata {
            dev_ids: vec!["dev1".into(), "dev2".into(), "dev3".into()],
            count: 4,
            samples: vec![
                rtdata_sample(
                    "dev1",
                    &[
                        ("temp", SampleValue::Float(1.5)),
                        ("hum", SampleValue::Int(60)),
                    ],
                ),
                rtdata_sample("dev2", &[("p", SampleValue::Str("on".into()))]),
                rtdata_sample("dev3", &[("x", SampleValue::Int(9))]),
            ],
            ts: "2026-08-20T00:00:00Z".into(),
        });
        if let Some(v) = kv_str(&handle.client(), "transform-2", "got_dev1_temp").await {
            got_temp = Some(v);
            if kv_str(&handle.client(), "transform-2", "got_dev2_p")
                .await
                .is_some()
            {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    assert_eq!(
        got_temp.as_deref(),
        Some("1.5"),
        "dev1.temp 应命中（tag glob temp*）：{got_temp:?}"
    );
    assert_eq!(
        kv_str(&handle.client(), "transform-2", "got_dev2_p")
            .await
            .as_deref(),
        Some("on"),
        "dev2 全 tag 订阅应命中 p（字符串值直传）"
    );
    // 宿主过滤：未命中点不投递（回调未写 kv）。
    assert!(
        kv_str(&handle.client(), "transform-2", "got_dev1_hum")
            .await
            .is_none(),
        "dev1.hum 不匹配 temp*，不应回调"
    );
    assert!(
        kv_str(&handle.client(), "transform-2", "got_dev3_x")
            .await
            .is_none(),
        "dev3 未订阅，不应回调"
    );

    stop_inst(app.clone(), &token, "transform-2").await;
    assert_eq!(mgr.status().get("transform-2"), Some(&InstState::Stopped));
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

/// manifest any-of 契约拒载（T5 e2e 面）：`@role transform` 既无 on_timer 也无
/// on_rtdata（只有 on_init）→ warn + run 提前返回 → supervisor 标 Failed（不进
/// Running）、stat 不写（tracker 未构造）。
#[tokio::test]
async fn transform_manifest_missing_callbacks_rejected() {
    let data_dir = common::test_data_dir("lua_transform_reject");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(16);
    let mgr = InstanceManager::new(test_ctx(client.clone(), EventBus::dummy()), hub_tx);

    let scripts = std::env::temp_dir().join(format!("lua-transform-rj-{}", std::process::id()));
    let script_dir = scripts.join("tbad");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        "-- @role transform\nfunction on_init() end\n",
    )
    .expect("write");

    let inst = transform_inst(
        "transform-bad",
        "luaTransform",
        serde_json::json!({
            "script": "tbad",
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
    );
    mgr.start_source(inst)
        .await
        .expect("start_source 本身成功（拒载在 task 内）");

    // run() 内拒载 → task 退出 → supervisor 标 Failed（轮询到稳定态，≤5s）。
    let mut final_state = None;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(s) = mgr.status().get("transform-bad") {
            if *s == InstState::Failed {
                final_state = Some(*s);
                break;
            }
        }
    }
    assert_eq!(
        final_state,
        Some(InstState::Failed),
        "缺 on_timer/on_rtdata 应拒载（warn + task 退出 → Failed，不进 Running）: {:?}",
        mgr.status()
    );
    // 拒载发生在 tracker 构造前 → stat 不得写入。
    let stat = rs_iot::db::stat::read(&client, "transform-bad")
        .await
        .expect("read stat");
    assert!(stat.is_none(), "拒载实例不应写 stat: {stat:?}");

    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

// ===== T8：自环防护（Q6 硬性验收）——真实链路 =====

/// HGET rtdata cell 原文（`[ts, localTime, value]` JSON 文本；空串 = 无此 field）。
async fn hget_cell(client: &lux::EmbeddedClient, key: &str, field: &str) -> String {
    use lux::EmbeddedValue;
    match client.execute_value("HGET", &[key, field]).await {
        Ok(EmbeddedValue::Bulk(b)) => String::from_utf8_lossy(&b).to_string(),
        Ok(EmbeddedValue::Simple(s)) => s,
        _ => String::new(),
    }
}

/// 真实链路测试底座（T8）：真实 EventBus + 真实 pipeline（hub → 50ms 攒批 → lux
/// 入库 + Rtdata publish 回 EventBus，`tests/lua_l2_e2e.rs` 的 `pipeline::run` 同款
/// 接线）——`set_value` 产出经生产路径回流订阅端，非手动等价 publish 模拟。
/// 返回 (mgr, event_bus, handle, shutdown)；`shutdown.cancel()` 停 pipeline。
#[allow(clippy::type_complexity)]
async fn real_chain(
    tag: &str,
) -> (
    Arc<InstanceManager>,
    EventBus,
    lux::ServerHandle,
    CancellationToken,
) {
    let data_dir = common::test_data_dir(tag);
    // 清历史残留（common::test_data_dir 不清理目录：上一轮运行留下的 luakv 会让
    // 精确断言（kv==1）读到脏值直接假红——先整删再重建，保证本轮从零开始）。
    let _ = std::fs::remove_dir_all(&data_dir);
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let event_bus = EventBus::new(64);
    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(Arc::new(rs_iot::process_metrics::ProcessMetrics::new())),
        bridge_registry: BridgeRegistry::new(),
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: event_bus.clone(),
    };
    // hub → pipeline（1 worker + Passthrough：T8 只验回流链路，无数据加工）。
    let (hub_tx, hub_rx) = mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));
    let proc: Arc<dyn Processor> = Arc::new(Passthrough);
    let _pipeline = tokio::spawn(rs_iot::pipeline::run(
        ctx,
        hub_rx,
        proc,
        1,
        shutdown.child_token(),
    ));
    (mgr, event_bus, handle, shutdown)
}

/// 自环防护（T8 硬性验收，Q6 路线图 C 行）：实例订阅自身会写的 tag（`devX.*`），
/// `on_rtdata` 里 `set_value` 写 devX.derived → 外部经真实 EventBus publish devX
/// 事件 → on_rtdata 触发一次（kv=1）→ 产出样本经真实链路（hub → pipeline 50ms
/// 攒批 → publish Rtdata）回流自身订阅 → self-write tracker 窗口内命中 → 跳过
/// 不投——断言 kv 计数稳定不增长（2.5s 轮询五采样）。
#[tokio::test]
async fn transform_self_loop_guard_suppresses_own_output() {
    let (mgr, event_bus, handle, shutdown) = real_chain("lua_transform_selfloop").await;

    let scripts = std::env::temp_dir().join(format!("lua-transform-loop-{}", std::process::id()));
    let script_dir = scripts.join("loop");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        r#"-- @role transform
function on_rtdata(devid, tag, value)
  local n = (ctx:kv_get("loop") or 0) + 1
  ctx:kv_set("loop", n)
  ctx:set_value("devX", "derived", n)
end
"#,
    )
    .expect("write");

    // period=0 纯订阅（无 on_timer，计数只来自 on_rtdata）；订阅面覆盖自身写点。
    mgr.start_source(transform_inst(
        "transform-loop",
        "luaTransform",
        serde_json::json!({
            "script": "loop",
            "period": 0,
            "subscribe": ["devX.*"],
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
    ))
    .await
    .expect("start_source");

    // 订阅建立竞态兜底（照上方 rtdata 测试模式）：重发 + 轮询直到 kv 可见（晚订阅
    // 前的发布自然丢，重发无害）；一旦可见即停发——后续计数增长只可能来自回流。
    let client = handle.client();
    let mut first: Option<i64> = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while first.is_none() && std::time::Instant::now() < deadline {
        event_bus.publish(EventPayload::Rtdata {
            dev_ids: vec!["devX".into()],
            count: 1,
            samples: vec![rtdata_sample("devX", &[("temp", SampleValue::Float(20.5))])],
            ts: "2026-08-20T00:00:00Z".into(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        if let Some(s) = kv_str(&client, "transform-loop", "loop").await {
            if let Ok(n) = s.parse::<i64>() {
                first = Some(n);
            }
        }
    }
    let baseline = first.expect("on_rtdata 应至少触发一次（kv loop 可见）");
    assert_eq!(
        baseline, 1,
        "单次外部 publish 应只触发一次 on_rtdata（kv=1）"
    );

    // 防假绿：devX.derived 已落 lux——set_value → hub → pipeline 真实回流发生过
    //（publish 与入库同一 flush，field 在即回流事件已发出，防护缺失则二次触发
    // 已在路上，下方稳定性窗口会捕获）。
    let mut derived = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while derived.is_empty() && std::time::Instant::now() < deadline {
        derived = hget_cell(&client, "rtdata:devX", "devX.derived").await;
        if derived.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    assert!(
        !derived.is_empty(),
        "派生点应经真实链路落库（防回流未发生的假绿）"
    );

    // 稳定性窗口：2.5s 五次采样 kv 恒等于 baseline（自身回流被窗口过滤，不触发）。
    for _ in 0..5 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let n = kv_str(&client, "transform-loop", "loop")
            .await
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(-1);
        assert_eq!(n, baseline, "自环防护应跳过自身回流，kv 计数稳定不增长");
    }

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

// ===== T9：agg-filter 示例实测（真实链路，硬性验收「一个清洗/聚合脚本上线」）=====

/// examples 下的示例脚本文件本体（include_str 读仓库文件——非内嵌副本，
/// 示例与实现漂移时本测试直接红，防示例腐化）。
const AGG_FILTER_EXAMPLE: &str = include_str!("../examples/lua-scripts/agg-filter/main.lua");

/// agg-filter 示例端到端（T9）：真实链路（EventBus publish → subscribe 宿主过滤 →
/// on_rtdata → set_value → hub → pipeline → lux 落库）跑通示例文件本体：
/// - 窗口大小可配（C4 kv 持久配置）：预写 kv `cfg_window=3`，on_init 读 kv 覆盖脚本
///   默认 5——窗口大小不是编译期常量；
/// - 限幅清洗：999 探针（超上限 120 被剔——探测订阅就绪且不污染窗口）+ 200 野点
///   → `rejected` 计数增长、不进窗口不产出；
/// - 滑动窗口均值：喂 10 → 20 → 200(野点) → 30 → 窗 [10,20,30]（窗口 3 恰满，
///   脚本默认 5 若未被 kv 覆盖则窗为 [10,20,30] 之外形态——cfg_window 生效的旁证）
///   → 派生点 `dev.agg.temp_avg` 落库值 = 20（均值），窗口 kv = [10,20,30]。
#[tokio::test]
async fn agg_filter_example_end_to_end() {
    let (mgr, event_bus, handle, shutdown) = real_chain("lua_transform_aggfilter").await;

    let scripts = std::env::temp_dir().join(format!("lua-transform-agg-{}", std::process::id()));
    let script_dir = scripts.join("agg-filter");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(script_dir.join("main.lua"), AGG_FILTER_EXAMPLE).expect("写示例文件本体");

    // C4 持久配置演示：预写 kv cfg_window=3（on_init 读取覆盖脚本默认 5）。
    rs_iot::db::luakv::set(&handle.client(), "agg-filter-1", "cfg_window", "3")
        .await
        .expect("预写 cfg_window");

    mgr.start_source(transform_inst(
        "agg-filter-1",
        "luaTransform",
        serde_json::json!({
            "script": "agg-filter",
            "period": 0,
            "subscribe": ["raw1.temp*"],
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
    ))
    .await
    .expect("start agg-filter");

    let client = handle.client();
    // 单点发布 helper：raw1.temp = v（devid 不含 "."——subscribe 模式以 "." 分段）。
    let pub_temp = |v: SampleValue| {
        event_bus.publish(EventPayload::Rtdata {
            dev_ids: vec!["raw1".into()],
            count: 1,
            samples: vec![rtdata_sample("raw1", &[("temp", v)])],
            ts: "2026-08-21T00:00:00Z".into(),
        });
    };

    // 订阅就绪探测：野点 999（超 CLAMP_HI=120 被剔除——探测点不进窗口），
    // rejected 计数可见即订阅已通（重发无害：野点幂等剔除）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        pub_temp(SampleValue::Int(999));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let rejected = kv_str(&client, "agg-filter-1", "rejected")
            .await
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        if rejected >= 1 || std::time::Instant::now() > deadline {
            break;
        }
    }
    assert!(
        std::time::Instant::now() <= deadline,
        "订阅就绪探测超时（rejected 计数始终不可见）"
    );
    // 探针停发缓冲：最后一批在途回调落地，再进数据面（避免与数据点交错）。
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // 数据面：10 → 20 → 200(野点) → 30。窗口 3：[10,20,30] → 均值 20。
    for v in [10, 20, 200, 30] {
        pub_temp(SampleValue::Int(v));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }

    // 派生点落库值 = 窗口均值 20（cell JSON [ts, localTime, value]——value 是字符串
    // 形态，write_batch 的 val_str 序列化语义，见 db::rtdata.rs）。
    let mut derived: f64 = f64::NAN;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let cell = hget_cell(&client, "rtdata:agg1", "agg1.temp_avg").await;
        if !cell.is_empty() {
            if let Some(v) = serde_json::from_str::<serde_json::Value>(&cell)
                .ok()
                .and_then(|arr| arr.get(2).and_then(|v| v.as_str()).map(str::to_string))
            {
                if let Ok(n) = v.parse::<f64>() {
                    derived = n;
                    break;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    assert_eq!(
        derived, 20.0,
        "派生点 agg1.temp_avg 应为窗口均值 20（实测 {derived}）"
    );

    // 窗口状态（kv 持久，C4）：恰 [10,20,30]——200 被剔除 + 窗口 3 生效（默认 5 会
    // 是 [10,20,30] 同形但语义靠 cfg_window 覆盖路径保证，见上方 cfg_window 预写）。
    assert_eq!(
        kv_str(&client, "agg-filter-1", "win").await.as_deref(),
        Some("[10,20,30]"),
        "滑动窗口应为 [10,20,30]（200 剔除 + 窗口大小 3）"
    );
    // 野点计数：探测期 ≥1 + 数据面 200 一笔 ≥2（探测重发次数不定，下界断言）。
    let rejected = kv_str(&client, "agg-filter-1", "rejected")
        .await
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    assert!(
        rejected >= 2,
        "野点剔除计数应 ≥2（探测 + 200，实测 {rejected}）"
    );
    // 自身输出不回流触发（订阅面 raw1.*，输出 agg1.* 不相交——无回调放大）：
    // rejected/win 在停发后稳定（500ms 双采样）。
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(
        kv_str(&client, "agg-filter-1", "win").await.as_deref(),
        Some("[10,20,30]"),
        "停发后窗口应稳定（无自环放大）"
    );

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

/// 跨实例不误伤（T8）：transform A（无订阅、period=1s，`on_timer` 里 `set_value`
/// 写 devB.derived）+ transform B（订阅 devB）→ A 的产出经真实链路（hub →
/// pipeline → publish Rtdata）到达 B 的 `on_rtdata`（kv 计数增长）——自环防护是
/// per-instance self-write 表（A 记 (devB,derived) 只挡 A 自己），不挡正常跨实例
/// 传播（单跳）。
#[tokio::test]
async fn transform_cross_instance_propagation_not_blocked() {
    let (mgr, _event_bus, handle, shutdown) = real_chain("lua_transform_cross").await;

    let scripts = std::env::temp_dir().join(format!("lua-transform-x-{}", std::process::id()));
    for (dir, src) in [
        (
            "a",
            r#"-- @role transform
function on_timer()
  local n = (ctx:kv_get("t") or 0) + 1
  ctx:kv_set("t", n)
  ctx:set_value("devB", "derived", n)
end
"#,
        ),
        (
            "b",
            r#"-- @role transform
function on_rtdata(devid, tag, value)
  local n = (ctx:kv_get("got") or 0) + 1
  ctx:kv_set("got", n)
  ctx:kv_set("last_tag", tag)
end
"#,
        ),
    ] {
        let script_dir = scripts.join(dir);
        std::fs::create_dir_all(&script_dir).expect("mkdir");
        std::fs::write(script_dir.join("main.lua"), src).expect("write");
    }

    mgr.start_source(transform_inst(
        "transform-a",
        "luaTransform",
        serde_json::json!({
            "script": "a",
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
    ))
    .await
    .expect("start A");
    mgr.start_source(transform_inst(
        "transform-b",
        "luaTransform",
        serde_json::json!({
            "script": "b",
            "period": 0,
            "subscribe": ["devB"],
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
    ))
    .await
    .expect("start B");

    // A 首 tick（jitter 相位 <1s，此后 1s 一拍）→ set_value → hub → pipeline
    // flush(50ms) → publish → B 订阅命中 → on_rtdata。轮询 B 的 kv got ≥1；
    // A 侧 kv t 可见佐证生产端真跑（非空即够）。
    let client = handle.client();
    let mut got: i64 = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Some(s) = kv_str(&client, "transform-b", "got").await {
            if let Ok(n) = s.parse::<i64>() {
                got = n;
                if n >= 1 {
                    break;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    assert!(
        got >= 1,
        "跨实例传播不误伤：B 的 on_rtdata 应收到 A 的产出（got 实测 {got}）"
    );
    assert_eq!(
        kv_str(&client, "transform-b", "last_tag").await.as_deref(),
        Some("derived"),
        "B 收到的应是 A 写的派生点 tag"
    );
    assert!(
        kv_str(&client, "transform-a", "t").await.is_some(),
        "A 的 on_timer 应真跑（生产端佐证）"
    );

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}
