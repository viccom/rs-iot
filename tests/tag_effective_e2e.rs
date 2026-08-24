//! 标签元数据 effective 视图 + devreg.overrides 写入端到端（M-4 T3；
//! Phase 1a tag_registry_e2e 六场景语义平移——tags 表退役）。
//!
//! ① JWT 保护；② effective 视图列出模型 tag（_source=model）；③ tag/set
//! 三态覆盖（缺省不动/null 恢复继承/值覆盖）+ 合并视图；④ 覆盖不被模板刷新
//! 冲掉（三态语义核心：改模板 → 未覆盖字段跟随、覆盖字段保留）；⑤ glob
//! 批量写 overrides；⑥ tag/delete 清覆盖回模板默认；⑦ 旧 tags 表只读导出
//! （升级过渡：数据丢弃前人工对照恢复）。
//!
//! 注意：全局 effective 缓存被并行测试的写路径刷新覆盖（M-1 已知竞态）——
//! 否定/精确断言用轮询重试收敛。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::event_bus::EventBus;
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::log_ringbuffer::LogRingBuffer;
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{Hwid, Sample};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 建测试 app：lux + pipeline（1 worker）+ InstanceManager + REST 路由
/// （元数据面为主，pipeline 留作数据链路背景）。返回 (router, lux, hub, mgr)。
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

/// 登录拿 JWT（admin/admin，同 model_registry_e2e 口径）。
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

async fn get(app: &axum::Router, tok: &str, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                .body(Body::empty())
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

/// 注册一台"隐式模型"设备并直接落模板 props（t1 带默认属性、t2 裸默认）。
/// devid 互不重叠（全局 effective 缓存并行竞态下按 key 收敛）。
async fn seed_device(handle: &lux::ServerHandle, app: &axum::Router, tok: &str, devid: &str) {
    use rs_iot::types::{AppType, ChildSpec, DeviceSpec, HostKind, InstConfig, PluginName};
    let model_id = format!("_inline_simtag_{devid}");
    let bindings = common::register_devices_as_models(
        &handle.client(),
        "simtag",
        "dataSim",
        vec![DeviceSpec {
            label: "d".into(),
            value: devid.into(),
            children: vec![
                ChildSpec {
                    label: "t1".into(),
                    value: "t1".into(),
                    r#type: "f32".into(),
                    props: None,
                },
                ChildSpec {
                    label: "t2".into(),
                    value: "t2".into(),
                    r#type: "f32".into(),
                    props: None,
                },
            ],
            slave_id: None,
        }],
    )
    .await;
    let inst = InstConfig {
        inst_id: format!("simtag_{devid}").into(),
        inst_name: "it".into(),
        app_code: PluginName::new("dataSim"),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({}),
        devices: bindings,
        host_kind: HostKind::Builtin,
    };
    let _ = inst; // 实例配置仅形式占位（本套件不启动 Source）
    let (st, v) = post(
        app,
        tok,
        "/api/v1/devmodel/update",
        &format!(
            r#"{{"model_id":"{model_id}","tags":[
                {{"name":"t1","data_type":"f32","unit":"kPa","props":{{"history":false,"deadband":0.5}}}},
                {{"name":"t2","data_type":"f32"}}]}}"#
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "模型模板更新: {v:?}");
}

/// 轮询 GET /tags 直到断言通过或超时（全局 effective 缓存竞态收敛）。
async fn tags_eventually(
    app: &axum::Router,
    tok: &str,
    query: &str,
    pred: impl Fn(&serde_json::Map<String, serde_json::Value>) -> bool,
) -> serde_json::Map<String, serde_json::Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let (st, v) = get(app, tok, &format!("/api/v1/tags{query}")).await;
        assert_eq!(st, StatusCode::OK, "{v:?}");
        if let serde_json::Value::Object(m) = v {
            if pred(&m) {
                return m;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "8s 内 effective 视图未收敛"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------- ① JWT 保护 ----------

#[tokio::test]
async fn tags_api_requires_jwt() {
    let (app, handle, _hub, _mgr) = test_app("tageff401").await;
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

// ---------- ② effective 视图列出模型 tag ----------

#[tokio::test]
async fn effective_view_lists_model_tags_with_source() {
    let (app, handle, _hub, _mgr) = test_app("tageff-view").await;
    let tok = jwt(&app).await;
    seed_device(&handle, &app, &tok, "evA").await;

    let m = tags_eventually(&app, &tok, "?dev_id=evA", |m| {
        m.get("evA.t1")
            .is_some_and(|p| p["_source"] == serde_json::json!("model"))
    })
    .await;
    let t1 = &m["evA.t1"];
    assert_eq!(t1["unit"], serde_json::json!("kPa"), "模板默认属性");
    assert_eq!(t1["deadband"], serde_json::json!(0.5));
    assert_eq!(t1["history"], serde_json::json!(false));
    assert!(m.contains_key("evA.t2"), "模型全部 tag 都在视图: {m:?}");
    common::stop_lux(handle).await;
}

// ---------- ③ tag/set 三态覆盖 + 合并视图 ----------

#[tokio::test]
async fn tag_set_three_state_override_and_merge() {
    let (app, handle, _hub, _mgr) = test_app("tageff-set").await;
    let tok = jwt(&app).await;
    seed_device(&handle, &app, &tok, "svA").await;

    // 值覆盖：history=true（模板 false）。
    let (st, v) = post(
        &app,
        &tok,
        "/api/v1/tag/set",
        r#"{"dev_id":"svA","tag":"t1","history":true}"#,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v:?}");
    let m = tags_eventually(&app, &tok, "?dev_id=svA", |m| {
        m.get("svA.t1")
            .is_some_and(|p| p["history"] == serde_json::json!(true))
    })
    .await;
    let t1 = &m["svA.t1"];
    assert_eq!(
        t1["_source"],
        serde_json::json!("override"),
        "有覆盖标 override"
    );
    assert_eq!(t1["unit"], serde_json::json!("kPa"), "未覆盖字段继承模板");

    // null 恢复继承：history 回模板 false。
    let (st, v) = post(
        &app,
        &tok,
        "/api/v1/tag/set",
        r#"{"dev_id":"svA","tag":"t1","history":null}"#,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v:?}");
    let m = tags_eventually(&app, &tok, "?dev_id=svA", |m| {
        m.get("svA.t1")
            .is_some_and(|p| p["history"] == serde_json::json!(false))
    })
    .await;
    assert_eq!(
        m["svA.t1"]["_source"],
        serde_json::json!("model"),
        "清覆盖回 model 源"
    );

    // 未建模 tag / 未注册设备 → 400/404。
    let (st, _) = post(
        &app,
        &tok,
        "/api/v1/tag/set",
        r#"{"dev_id":"svA","tag":"nope","history":true}"#,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "tag 不在模型");
    let (st, _) = post(
        &app,
        &tok,
        "/api/v1/tag/set",
        r#"{"dev_id":"nosuch","tag":"t1","history":true}"#,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "设备不在 devreg");
    common::stop_lux(handle).await;
}

// ---------- ④ 覆盖不被模板刷新冲掉 ----------

#[tokio::test]
async fn override_survives_template_refresh() {
    let (app, handle, _hub, _mgr) = test_app("tageff-keep").await;
    let tok = jwt(&app).await;
    let model_id = "_inline_simtag_kpA".to_string();
    seed_device(&handle, &app, &tok, "kpA").await;

    // 单台覆盖 deadband=9.9。
    let (st, v) = post(
        &app,
        &tok,
        "/api/v1/tag/set",
        r#"{"dev_id":"kpA","tag":"t1","deadband":9.9}"#,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v:?}");

    // 模板刷新：改模型默认 deadband=1.0 + unit 换标。
    let (st, v) = post(
        &app,
        &tok,
        "/api/v1/devmodel/update",
        &format!(
            r#"{{"model_id":"{model_id}","tags":[
                {{"name":"t1","data_type":"f32","unit":"MPa","props":{{"history":true,"deadband":1.0}}}},
                {{"name":"t2","data_type":"f32"}}]}}"#
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v:?}");

    // 覆盖字段保留 9.9；未覆盖字段（unit/history）跟随模板新值。
    let m = tags_eventually(&app, &tok, "?dev_id=kpA", |m| {
        m.get("kpA.t1").is_some_and(|p| {
            p["deadband"] == serde_json::json!(9.9)
                && p["unit"] == serde_json::json!("MPa")
                && p["history"] == serde_json::json!(true)
        })
    })
    .await;
    assert_eq!(m["kpA.t1"]["_source"], serde_json::json!("override"));
    common::stop_lux(handle).await;
}

// ---------- ⑤ glob 批量写 overrides ----------

#[tokio::test]
async fn batch_glob_writes_overrides() {
    let (app, handle, _hub, _mgr) = test_app("tageff-batch").await;
    let tok = jwt(&app).await;
    seed_device(&handle, &app, &tok, "baA").await;
    seed_device(&handle, &app, &tok, "baB").await;

    let (st, v) = post(
        &app,
        &tok,
        "/api/v1/tag/batch",
        r#"{"pattern":"ba*.t1","history":true}"#,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v:?}");
    assert_eq!(v["matched"], serde_json::json!(2), "{v:?}");
    assert_eq!(v["updated"], serde_json::json!(2), "{v:?}");

    let m = tags_eventually(&app, &tok, "?history=true", |m| {
        m.len() == 2
            && m.get("baA.t1")
                .is_some_and(|p| p["history"] == serde_json::json!(true))
            && m.get("baB.t1")
                .is_some_and(|p| p["history"] == serde_json::json!(true))
    })
    .await;
    assert!(m.contains_key("baA.t1") && m.contains_key("baB.t1"));
    assert!(!m.contains_key("baA.t2"), "t2 不匹配 pattern");
    common::stop_lux(handle).await;
}

// ---------- ⑥ delete 清覆盖回模板默认 ----------

#[tokio::test]
async fn delete_clears_override_back_to_template() {
    let (app, handle, _hub, _mgr) = test_app("tageff-del").await;
    let tok = jwt(&app).await;
    seed_device(&handle, &app, &tok, "dlA").await;

    let (st, _) = post(
        &app,
        &tok,
        "/api/v1/tag/set",
        r#"{"dev_id":"dlA","tag":"t1","history":true,"deadband":7.7}"#,
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, v) = post(
        &app,
        &tok,
        "/api/v1/tag/delete",
        r#"{"dev_id":"dlA","tag":"t1"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v:?}");
    // 回模板默认（history=false/deadband=0.5）且 tag 仍在视图（模型 tag 不删）。
    let m = tags_eventually(&app, &tok, "?dev_id=dlA", |m| {
        m.get("dlA.t1").is_some_and(|p| {
            p["history"] == serde_json::json!(false)
                && p["deadband"] == serde_json::json!(0.5)
                && p["_source"] == serde_json::json!("model")
        })
    })
    .await;
    assert!(m.contains_key("dlA.t1"), "模型 tag 不从视图消失");
    common::stop_lux(handle).await;
}

// ---------- ⑦ 旧 tags 表只读导出（升级过渡） ----------

#[tokio::test]
async fn legacy_export_reads_old_tags_table() {
    let (app, handle, _hub, _mgr) = test_app("tageff-legacy").await;
    let tok = jwt(&app).await;
    // 直写旧表两条（模拟升级前运行期数据）。
    let client = handle.client();
    for (k, props) in [
        ("lgA.x", r#"{"history":true,"owner_inst":"prev"}"#),
        ("lgA.y", r#"{"deadband":2.0}"#),
    ] {
        client.execute("HSET", &["tags", k, props]).await.unwrap();
    }
    let (st, v) = get(&app, &tok, "/api/v1/tags/legacy-export").await;
    assert_eq!(st, StatusCode::OK, "{v:?}");
    assert_eq!(v["lgA.x"]["history"], serde_json::json!(true));
    assert_eq!(v["lgA.x"]["owner_inst"], serde_json::json!("prev"));
    assert_eq!(v["lgA.y"]["deadband"], serde_json::json!(2.0));
    assert_eq!(v.as_object().unwrap().len(), 2);
    // 导出是只读快照：不随 effective 写入变化。
    post(
        &app,
        &tok,
        "/api/v1/tag/set",
        r#"{"dev_id":"lgA","tag":"x","history":true}"#,
    )
    .await;
    let (_, v2) = get(&app, &tok, "/api/v1/tags/legacy-export").await;
    assert_eq!(v, v2, "只读");
    common::stop_lux(handle).await;
}
