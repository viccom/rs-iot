//! Tag Registry 端到端：三路径登记语义 + REST 面 + 只补缺不覆盖 + 删除重占位。
//!
//! 覆盖（docs/designs/2026-08-24-tag-registry-plan.md Task 6 六场景）：
//! ① JWT 保护；② dataSim 全链路自动登记（默认 props + owner_inst=宿主实例）；
//! ③ 声明式 props 初值（instances 配置 + refresh_declared → 首见建卡落初值）；
//! ④ 只补缺不覆盖（表已有记录——API/前次运行——不被出口登记冲掉）；
//! ⑤ glob 批量（matched/updated 计数 + 未匹配不动）；
//! ⑥ 删除后重占位（seen 移除 → 再出数默认卡重建）。
//!
//! 注意：`SEEN` 是进程级全局，测试并行共享——**各测试用互不重叠的 devid**
//! （devA/devC/devD/devE/devF/devG）避免互扰。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::event_bus::EventBus;
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::log_ringbuffer::LogRingBuffer;
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{
    AppType, ChildSpec, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample, SampleValue,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 建测试 app：lux + pipeline（1 worker）+ InstanceManager + REST 路由。
/// 骨架同 rtdata_subscribe_e2e，加真实 pipeline（dataflow.rs 模式）使
/// flush → note_once 全链路生效。返回 (router, lux handle, hub_tx, mgr)。
async fn test_app(
    name: &str,
) -> (
    axum::Router,
    lux::ServerHandle,
    tokio::sync::mpsc::Sender<Sample>,
    Arc<InstanceManager>,
) {
    let data_dir = common::test_data_dir(name);
    if data_dir.exists() {
        std::fs::remove_dir_all(&data_dir).expect("清空上轮测试 data_dir");
    }
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let shutdown_for_pipeline = shutdown.clone();
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
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx.clone()));
    let mgr_ret = mgr.clone();
    let processor: Arc<dyn rs_iot::plugin::traits::Processor> = Arc::new(Passthrough);
    tokio::spawn(rs_iot::pipeline::run(
        ctx.clone(),
        hub_rx,
        processor,
        1,
        shutdown_for_pipeline,
    ));

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
        config: Arc::new(rs_iot::config::AppConfig::default()),
    };
    (build_router(state), handle, hub_tx, mgr_ret)
}

/// 登录拿 JWT（admin/admin，同 subscribe e2e）。
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

/// 直推一条 Sample 进 hub（绕过 Source，直达 pipeline flush 路径）。
fn sample(devid: &str, tag: &str, val: f64) -> Sample {
    let mut values = BTreeMap::new();
    values.insert(tag.to_string(), SampleValue::Float(val));
    Sample {
        hwid: Hwid::new("it-hwid"),
        devid: devid.to_string(),
        values,
        timestamp: 1787600000,
    }
}

/// GET /api/v1/tags（带过滤参数）→ 解析后的 map。
async fn tags(
    app: &axum::Router,
    tok: &str,
    query: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/tags{query}"))
                .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
        .await
        .unwrap();
    match serde_json::from_slice::<serde_json::Value>(&bytes).unwrap() {
        serde_json::Value::Object(m) => m,
        other => panic!("expected object, got {other:?}"),
    }
}

async fn post(
    app: &axum::Router,
    tok: &str,
    uri: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let code = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        code,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

// ---------- ① JWT 保护 ----------

#[tokio::test]
async fn tags_api_requires_jwt() {
    let (app, handle, _hub, _mgr) = test_app("tagreg401").await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/tags")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    drop(resp);
    common::stop_lux(handle).await;
}

// ---------- ② dataSim 全链路自动登记 ----------

#[tokio::test]
async fn datasim_flow_registers_default_cards_with_owner() {
    let (app, handle, hub, mgr) = test_app("tagreg-flow").await;
    // M-2 模型驱动：devA 注册为隐式模型（dataSim 纯软：tag=temp/humidity）。
    let bindings = common::register_devices_as_models(
        &handle.client(),
        "sim_flow",
        "dataSim",
        vec![DeviceSpec {
            label: "d".into(),
            value: "devA".into(),
            children: vec![
                ChildSpec {
                    label: "t".into(),
                    value: "temp".into(),
                    r#type: "float".into(),
                    props: None,
                },
                ChildSpec {
                    label: "h".into(),
                    value: "humidity".into(),
                    r#type: "float".into(),
                    props: None,
                },
            ],
            slave_id: None,
        }],
    )
    .await;
    let inst = InstConfig {
        inst_id: "sim_flow".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("dataSim"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({ "period": 1 }),
        devices: bindings,
        host_kind: HostKind::Builtin,
    };
    drop(hub); // 释放直推通道（本测试走真实 Source）
    mgr.start_source(inst).await.expect("start datasim");
    tokio::time::sleep(Duration::from_millis(2300)).await; // ≥2 个采集周期

    let tok = jwt(&app).await;
    let m = tags(&app, &tok, "?dev_id=devA").await;
    assert!(m.contains_key("devA.temp"), "自动登记: {m:?}");
    assert!(m.contains_key("devA.humidity"));
    let props = &m["devA.temp"];
    assert_eq!(props["history"], serde_json::json!(false), "默认不归档");
    assert_eq!(
        props["owner_inst"],
        serde_json::json!("sim_flow"),
        "owner=宿主实例（dev2inst 首宿主）"
    );
    common::stop_lux(handle).await;
}

// ---------- ③ 声明式 props 初值 ----------

#[tokio::test]
async fn declared_props_seed_initial_card() {
    let (app, handle, hub, _mgr) = test_app("tagreg-decl").await;
    // M-2：声明式 props 初值 = 模型模板默认（devG 建隐式模型，temp2 模板 props
    // 带声明值）+ refresh_declared（= effective 装载，模拟 startinst 接线）。
    let bindings = common::register_devices_as_models(
        &handle.client(),
        "sim_decl",
        "dataSim",
        vec![DeviceSpec {
            label: "d".into(),
            value: "devG".into(),
            children: vec![ChildSpec {
                label: "炉温".into(),
                value: "temp2".into(),
                r#type: "f32".into(),
                props: Some(rs_iot::types::TagProps {
                    history: true,
                    deadband: Some(0.5),
                    desc: None,
                    unit: Some("°C".into()),
                    owner_inst: String::new(),
                }),
            }],
            slave_id: None,
        }],
    )
    .await;
    let inst = InstConfig {
        inst_id: "sim_decl".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("dataSim"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({}),
        devices: bindings,
        host_kind: HostKind::Builtin,
    };
    let client = handle.client();
    client
        .execute(
            "HSET",
            &[
                "instances",
                "sim_decl",
                &serde_json::to_string(&inst).unwrap(),
            ],
        )
        .await
        .unwrap();
    rs_iot::db::tag_registry::refresh_declared(&client)
        .await
        .expect("refresh declared");
    // 直推样本（tag=temp2 与声明 value 候选一致）→ flush 建卡用声明初值。
    // 全局 effective 缓存被并行测试的写路径刷新覆盖（M-1 已知竞态）：轮询重试
    // ——reload 缓存 + 重推 + 重读，直到初值生效或超时（8s）。
    let tok = jwt(&app).await;
    let mut history_ok = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        rs_iot::db::tag_registry::refresh_declared(&client)
            .await
            .expect("refresh declared");
        hub.send(sample("devG", "temp2", 42.0)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let m = tags(&app, &tok, "").await;
        if let Some(props) = m.get("devG.temp2") {
            if props["history"] == serde_json::json!(true) {
                history_ok = true;
                break;
            }
        }
    }
    assert!(
        history_ok,
        "声明 history 生效（经 effective 初值→出口登记），8s 内未达成"
    );
    let m = tags(&app, &tok, "").await;
    let props = &m["devG.temp2"];
    assert_eq!(props["deadband"], serde_json::json!(0.5));
    assert_eq!(props["unit"], serde_json::json!("°C"));
    common::stop_lux(handle).await;
}

// ---------- ④ 只补缺不覆盖 ----------

#[tokio::test]
async fn existing_card_not_overwritten_by_exit_registration() {
    let (app, handle, hub, _mgr) = test_app("tagreg-keep").await;
    let client = handle.client();
    // 模拟"表已有记录"（前次运行/API 设置）：devC.x 直写 lux（不经 set()，不入
    // seen——正好覆盖 note_once 的 HGET-exists 分支）。
    let kept = rs_iot::types::TagProps {
        history: true,
        deadband: Some(2.0),
        owner_inst: "prev_run".into(),
        ..Default::default()
    };
    client
        .execute(
            "HSET",
            &["tags", "devC.x", &serde_json::to_string(&kept).unwrap()],
        )
        .await
        .unwrap();
    // 出口登记同 key → 必须原样保留。
    hub.send(sample("devC", "x", 1.0)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;

    let tok = jwt(&app).await;
    let m = tags(&app, &tok, "").await;
    let props = &m["devC.x"];
    assert_eq!(props["history"], serde_json::json!(true), "不被默认卡覆盖");
    assert_eq!(props["owner_inst"], serde_json::json!("prev_run"));
    common::stop_lux(handle).await;
}

// ---------- ⑤ glob 批量 ----------

#[tokio::test]
async fn batch_glob_updates_matched_only() {
    let (app, handle, hub, _mgr) = test_app("tagreg-batch").await;
    hub.send(sample("devD", "a", 1.0)).await.unwrap();
    hub.send(sample("devD", "b", 2.0)).await.unwrap();
    hub.send(sample("devE", "a", 3.0)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;

    let tok = jwt(&app).await;
    let (code, v) = post(
        &app,
        &tok,
        "/api/v1/tag/batch",
        r#"{"pattern":"devD.*","history":true,"deadband":1.5}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v:?}");
    assert_eq!(v["matched"], serde_json::json!(2), "{v:?}");
    assert_eq!(v["updated"], serde_json::json!(2), "{v:?}");

    let m = tags(&app, &tok, "?history=true").await;
    assert_eq!(m.len(), 2, "只有 devD.* 两卡: {m:?}");
    assert!(m.contains_key("devD.a") && m.contains_key("devD.b"));
    assert_eq!(m["devD.a"]["deadband"], serde_json::json!(1.5));
    common::stop_lux(handle).await;
}

// ---------- ⑥ 删除后重占位 ----------

#[tokio::test]
async fn delete_then_reflow_recreates_default_card() {
    let (app, handle, hub, _mgr) = test_app("tagreg-del").await;
    hub.send(sample("devF", "x", 1.0)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let tok = jwt(&app).await;

    // 人工置 history=true → 删除（seen 同步移除）→ 再出数 → 重建默认卡。
    let (code, v) = post(
        &app,
        &tok,
        "/api/v1/tag/set",
        r#"{"dev_id":"devF","tag":"x","history":true}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v:?}");
    let (code, v) = post(
        &app,
        &tok,
        "/api/v1/tag/delete",
        r#"{"dev_id":"devF","tag":"x"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v:?}");
    let m = tags(&app, &tok, "?dev_id=devF").await;
    assert!(m.is_empty(), "删除后无记录: {m:?}");

    hub.send(sample("devF", "x", 2.0)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let m = tags(&app, &tok, "?dev_id=devF").await;
    let props = &m["devF.x"];
    assert_eq!(props["history"], serde_json::json!(false), "重占位=默认卡");
    common::stop_lux(handle).await;
}
