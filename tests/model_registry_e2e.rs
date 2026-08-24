//! 设备模型注册表端到端（M-1 起累加；计划 §五 T4/T6）。
//!
//! T6 场景：401 保护 / devmodel 建查删 + 重名 409 + 被引用 409 / devreg
//! 引用完整性 4xx 矩阵 / 列表过滤 / effective 摘要 / 级联删除。
//! 注：设备端点走过渡路径 `/api/v1/devreg*`（`/api/v1/devices|/device/*`
//! 现由 routes_device 占用——M-2 InstConfig 切换时换路径并更新 MCP/前端）。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::event_bus::EventBus;
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::log_ringbuffer::LogRingBuffer;
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{Hwid, Sample};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 建测试 app：lux + InstanceManager + REST 路由（模型面无数据链路，不起
/// pipeline）。data_dir 先整删（lux WAL 持久化跨运行残留——重复跑同套件时
/// 旧数据会让 create-only 断言误判；lua_transform_e2e real_chain 同款先整删）。
/// 返回 (router, lux handle)。
async fn test_app(name: &str) -> (axum::Router, lux::ServerHandle) {
    let data_dir = common::test_data_dir(name);
    if data_dir.exists() {
        std::fs::remove_dir_all(&data_dir).expect("清空上轮测试 data_dir");
    }
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let metrics = Arc::new(rs_iot::process_metrics::ProcessMetrics::new());
    let event_bus = EventBus::new(256);
    let trace_registry = rs_iot::io_trace::TraceRegistry::with_sender(event_bus.clone());
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(metrics.clone()),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: metrics.clone(),
        trace_registry: trace_registry.clone(),
        event_tx: event_bus.clone(),
    };
    let (hub_tx, _hub_rx) = tokio::sync::mpsc::channel::<Sample>(16);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));

    let state = AppState {
        lux: client.clone(),
        mgr,
        hwid: Hwid::new("it-hwid"),
        jwt_secret: "test-secret-0123456789abcdef".into(),
        version: "itest",
        metrics: metrics.clone(),
        trace_registry,
        log_ringbuffer: Arc::new(LogRingBuffer::new(64)),
        event_bus,
        config: Arc::new(rs_iot::config::AppConfig::default()),
    };
    (build_router(state), handle)
}

/// 登录拿 JWT（admin/admin，同 tag_registry_e2e 口径）。
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
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["token"].as_str().unwrap().to_string()
}

/// 带认证 JSON POST（T6 场景 helper）。
async fn post_json(
    app: &axum::Router,
    token: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, v)
}

/// 带认证 GET（T6 场景 helper）。
async fn get_json(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, v)
}

/// 骨架冒烟：登录链路可用（模型 REST 场景的前置依赖）。
#[tokio::test]
async fn skeleton_login_smoke() {
    let (app, _handle) = test_app("model-skeleton").await;
    let token = jwt(&app).await;
    assert!(!token.is_empty());
}

// ===== T6 REST 场景 =====

fn model_body(model_id: &str) -> serde_json::Value {
    serde_json::json!({
        "model_id": model_id,
        "vendor": "威胜",
        "params": ["base"],
        "tags": [
            {"name": "ua", "data_type": "f32", "addr": "holding:{base}:f32",
             "rw": "r", "props": {"history": true, "deadband": 0.5}},
            {"name": "ub", "data_type": "f32", "addr": "holding:{base}+2:f32"}
        ]
    })
}

fn device_body(devid: &str, model: &str) -> serde_json::Value {
    serde_json::json!({
        "devid": devid,
        "model": model,
        "params": {"base": "100"},
        "overrides": {"ua": {"deadband": 2.0}}
    })
}

/// 401：模型/设备注册表端点默认 JWT 保护。
#[tokio::test]
async fn model_endpoints_require_jwt() {
    let (app, _handle) = test_app("model-401").await;
    for (method, uri) in [
        ("GET", "/api/v1/devmodels"),
        ("POST", "/api/v1/devmodel/set"),
        ("POST", "/api/v1/devmodel/update"),
        ("POST", "/api/v1/devmodel/delete"),
        ("GET", "/api/v1/devreg"),
        ("POST", "/api/v1/devreg/set"),
        ("POST", "/api/v1/devreg/delete"),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} 应 401"
        );
    }
}

/// devmodel 建查删主流程 + ?model_id= 单查。
#[tokio::test]
async fn devmodel_create_get_delete_flow() {
    let (app, _handle) = test_app("model-crud").await;
    let token = jwt(&app).await;

    let (st, _) = post_json(&app, &token, "/api/v1/devmodel/set", model_body("m_flow")).await;
    assert_eq!(st, StatusCode::OK, "建模型");

    let (st, v) = get_json(&app, &token, "/api/v1/devmodels").await;
    assert_eq!(st, StatusCode::OK);
    assert!(v.get("m_flow").is_some(), "列表含新模型：{v}");
    assert_eq!(v["m_flow"]["params"][0], "base");
    assert_eq!(v["m_flow"]["tags"][0]["name"], "ua");

    let (st, v) = get_json(&app, &token, "/api/v1/devmodels?model_id=m_flow").await;
    assert_eq!(st, StatusCode::OK);
    assert!(v.get("m_flow").is_some(), "单查过滤");

    let (st, _) = get_json(&app, &token, "/api/v1/devmodels?model_id=missing").await;
    assert_eq!(st, StatusCode::OK);
    assert!(v.get("missing").is_none());

    let (st, _) = post_json(
        &app,
        &token,
        "/api/v1/devmodel/delete",
        serde_json::json!({"model_id": "m_flow"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "删模型");

    let (_, v) = get_json(&app, &token, "/api/v1/devmodels").await;
    assert!(v.get("m_flow").is_none(), "删后不可见");

    // 再删 → 404。
    let (st, _) = post_json(
        &app,
        &token,
        "/api/v1/devmodel/delete",
        serde_json::json!({"model_id": "m_flow"}),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "删不存在 404");
}

/// 重名 409（create-only 语义）。
#[tokio::test]
async fn devmodel_duplicate_create_conflict() {
    let (app, _handle) = test_app("model-409").await;
    let token = jwt(&app).await;
    let (st, _) = post_json(&app, &token, "/api/v1/devmodel/set", model_body("m_dup")).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = post_json(&app, &token, "/api/v1/devmodel/set", model_body("m_dup")).await;
    assert_eq!(st, StatusCode::CONFLICT, "重名建 409");
}

/// 被引用 409 + 引用列表（删除守卫）。
#[tokio::test]
async fn devmodel_delete_referenced_conflict() {
    let (app, _handle) = test_app("model-ref409").await;
    let token = jwt(&app).await;
    post_json(&app, &token, "/api/v1/devmodel/set", model_body("m_ref")).await;
    let (st, _) = post_json(
        &app,
        &token,
        "/api/v1/devreg/set",
        device_body("d_ref", "m_ref"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "建设备");

    let (st, v) = post_json(
        &app,
        &token,
        "/api/v1/devmodel/delete",
        serde_json::json!({"model_id": "m_ref"}),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "被引用拒删");
    let refs = v["referenced_by"].as_array().expect("引用列表");
    assert!(refs.iter().any(|r| r == "d_ref"), "引用列表含 d_ref：{v}");
}

/// 模型结构非法 → 400（重复 tag 名）。
#[tokio::test]
async fn devmodel_invalid_create_bad_request() {
    let (app, _handle) = test_app("model-400").await;
    let token = jwt(&app).await;
    let mut bad = model_body("m_bad");
    bad["tags"][1]["name"] = serde_json::json!("ua"); // 重复
    let (st, _) = post_json(&app, &token, "/api/v1/devmodel/set", bad).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

/// devreg 引用完整性 4xx 矩阵：模型不存在 / 参数不齐 / 未知覆盖 tag / 非法 devid。
#[tokio::test]
async fn devreg_reference_matrix_4xx() {
    let (app, _handle) = test_app("devreg-4xx").await;
    let token = jwt(&app).await;
    post_json(&app, &token, "/api/v1/devmodel/set", model_body("m_dev")).await;

    // 模型不存在 → 400。
    let (st, v) = post_json(
        &app,
        &token,
        "/api/v1/devreg/set",
        device_body("d1", "nope"),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "未知模型 400：{v}");

    // 参数不齐 → 400。
    let mut missing = device_body("d1", "m_dev");
    missing["params"] = serde_json::json!({});
    let (st, _) = post_json(&app, &token, "/api/v1/devreg/set", missing).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "缺参 400");

    // 未知覆盖 tag → 400。
    let mut unk = device_body("d1", "m_dev");
    unk["overrides"] = serde_json::json!({"nope": {"history": true}});
    let (st, _) = post_json(&app, &token, "/api/v1/devreg/set", unk).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "未知覆盖 400");

    // 非法 devid（空白）→ 400。
    let mut badid = device_body("d 1", "m_dev");
    badid["devid"] = serde_json::json!("d 1");
    let (st, _) = post_json(&app, &token, "/api/v1/devreg/set", badid).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "非法 devid 400");

    // 全部失败后设备不入表。
    let (_, v) = get_json(&app, &token, "/api/v1/devreg").await;
    assert!(v.as_object().unwrap().is_empty(), "零设备：{v}");
}

/// devreg 列表/过滤 + effective 摘要 + 删除级联。
#[tokio::test]
async fn devreg_list_filter_effective_and_delete() {
    let (app, handle) = test_app("devreg-list").await;
    let token = jwt(&app).await;
    post_json(&app, &token, "/api/v1/devmodel/set", model_body("m_a")).await;
    post_json(
        &app,
        &token,
        "/api/v1/devreg/set",
        device_body("dA1", "m_a"),
    )
    .await;
    post_json(
        &app,
        &token,
        "/api/v1/devreg/set",
        device_body("dA2", "m_a"),
    )
    .await;

    // ?model= 过滤。
    let (st, v) = get_json(&app, &token, "/api/v1/devreg?model=m_a").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v.as_object().unwrap().len(), 2);
    // effective 摘要：ua 覆盖 deadband=2.0（模板 0.5）；ub 继承模板默认。
    let eff = &v["dA1"]["effective"];
    assert_eq!(eff["ua"]["deadband"], serde_json::json!(2.0), "覆盖生效");
    assert_eq!(
        eff["ub"]["deadband"],
        serde_json::Value::Null,
        "ub 模板默认无死区（继承语义）"
    );
    assert_eq!(eff["ua"]["history"], true);

    // ?devid= 单查。
    let (_, v) = get_json(&app, &token, "/api/v1/devreg?devid=dA2").await;
    assert_eq!(v.as_object().unwrap().len(), 1);

    // 删除级联：rtdata 一并清理。
    handle
        .client()
        .execute("HSET", &["rtdata:dA1", "dA1.ua", "[1,1,42]"])
        .await
        .unwrap();
    let (st, _) = post_json(
        &app,
        &token,
        "/api/v1/devreg/delete",
        serde_json::json!({"devid": "dA1"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, v) = get_json(&app, &token, "/api/v1/devreg").await;
    assert!(v.get("dA1").is_none() && v.get("dA2").is_some());
    let exists = handle
        .client()
        .execute_value("EXISTS", &["rtdata:dA1"])
        .await
        .unwrap();
    assert!(
        !matches!(exists, lux::EmbeddedValue::Int(n) if n != 0),
        "rtdata 级联清理"
    );

    // 再删 → 404。
    let (st, _) = post_json(
        &app,
        &token,
        "/api/v1/devreg/delete",
        serde_json::json!({"devid": "dA1"}),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// 一次性迁移：旧内联实例 → 隐式模型 + devreg 条目；幂等重跑零转换。
#[tokio::test]
async fn migrate_inline_devices_idempotent() {
    let (app, handle) = test_app("migrate").await;
    let token = jwt(&app).await;

    // 种旧式实例（modbus 内联点表，带 props 初值与 slave_id）。
    let legacy = serde_json::json!({
        "inst_id": "mb1", "app_code": "modbusClient", "app_type": "south",
        "app_config": {"endpoint": "127.0.0.1:1502"},
        "devices": [{
            "label": "1号表", "value": "mvx1",
            "children": [
                {"label": "温度", "value": "holding:0:f32", "type": "f32",
                 "props": {"history": true}},
                {"label": "电压", "value": "holding:2:f32", "type": "f32"}
            ],
            "slave_id": 3
        }]
    });
    handle
        .client()
        .execute("HSET", &["instances", "mb1", &legacy.to_string()])
        .await
        .unwrap();

    // 首跑：1 模型 + 1 设备 + 1 实例。
    let (st, v) = post_json(
        &app,
        &token,
        "/api/v1/migrate/inline-devices",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "迁移成功：{v}");
    assert_eq!(v["converted_models"], 1, "{v}");
    assert_eq!(v["converted_devices"], 1, "{v}");
    assert_eq!(v["converted_instances"], 1, "{v}");

    // 新结构生效（可采集前验证）：模型 addr 原样、props 带入；设备入册。
    let (_, m) = get_json(&app, &token, "/api/v1/devmodels?model_id=_inline_mb1_mvx1").await;
    assert!(m.get("_inline_mb1_mvx1").is_some(), "隐式模型已建：{m}");
    let model = &m["_inline_mb1_mvx1"];
    assert_eq!(model["tags"][0]["addr"], "holding:0:f32");
    assert_eq!(model["tags"][0]["props"]["history"], true);
    let (_, d) = get_json(&app, &token, "/api/v1/devreg?devid=mvx1").await;
    assert!(d.get("mvx1").is_some(), "设备入册：{d}");
    assert_eq!(d["mvx1"]["model"], "_inline_mb1_mvx1");
    // effective 摘要可算（模板默认流转）。
    assert_eq!(d["mvx1"]["effective"]["温度"]["history"], true);

    // 幂等重跑：零转换。
    let (st, v) = post_json(
        &app,
        &token,
        "/api/v1/migrate/inline-devices",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["converted_models"], 0, "重跑零转换：{v}");
    assert_eq!(v["converted_devices"], 0, "{v}");
    assert_eq!(v["converted_instances"], 0, "{v}");
}

/// devmodel/update：改模板 → 引用设备 effective 跟随；破坏引用完整性 → 409 带冲突明细。
#[tokio::test]
async fn devmodel_update_propagates_and_conflict_guards() {
    let (app, _handle) = test_app("model-update").await;
    let token = jwt(&app).await;
    post_json(&app, &token, "/api/v1/devmodel/set", model_body("m_upd")).await;
    post_json(
        &app,
        &token,
        "/api/v1/devreg/set",
        device_body("d_upd", "m_upd"),
    )
    .await;

    // ① 更新不存在 → 404。
    let (st, _) = post_json(&app, &token, "/api/v1/devmodel/update", model_body("nope")).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "更新不存在模型 404");

    // ② 合法更新：加 tag uc + ua 模板 deadband 0.5→1.0 → 设备 effective 跟随。
    let mut updated = model_body("m_upd");
    updated["tags"][0]["props"]["deadband"] = serde_json::json!(1.0);
    updated["tags"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "name": "uc", "data_type": "u16", "addr": "holding:{base}+4:u16"
        }));
    let (st, _) = post_json(&app, &token, "/api/v1/devmodel/update", updated).await;
    assert_eq!(st, StatusCode::OK, "合法更新");
    let (_, d) = get_json(&app, &token, "/api/v1/devreg?devid=d_upd").await;
    assert_eq!(
        d["d_upd"]["effective"]["uc"]["history"], false,
        "新 tag 出现在设备 effective：{d}"
    );
    let (_, m) = get_json(&app, &token, "/api/v1/devmodels?model_id=m_upd").await;
    assert_eq!(
        m["m_upd"]["tags"].as_array().unwrap().len(),
        3,
        "模型已更新"
    );

    // ③ 新增必填参数而设备未提供 → 409 带冲突明细。
    let mut bad_params = model_body("m_upd");
    bad_params["params"]
        .as_array_mut()
        .unwrap()
        .push("extra".into());
    let (st, v) = post_json(&app, &token, "/api/v1/devmodel/update", bad_params).await;
    assert_eq!(st, StatusCode::CONFLICT, "破坏 params 齐套 409：{v}");
    let conflicts = v["conflicts"].as_array().expect("冲突明细");
    assert!(
        conflicts.iter().any(|c| c["devid"] == "d_upd"),
        "冲突列出违规设备：{v}"
    );
    // ④ 删除被设备覆盖的 tag → 409。
    let mut bad_tags = model_body("m_upd");
    bad_tags["params"] = serde_json::json!(["base"]);
    bad_tags["tags"].as_array_mut().unwrap().remove(0); // 删掉 ua（device_body 覆盖了 ua）
    let (st, _) = post_json(&app, &token, "/api/v1/devmodel/update", bad_tags).await;
    assert_eq!(st, StatusCode::CONFLICT, "删被覆盖 tag 409");
    // ⑤ 两次 409 后模型保持 ② 的合法状态。
    let (_, m) = get_json(&app, &token, "/api/v1/devmodels?model_id=m_upd").await;
    assert_eq!(
        m["m_upd"]["tags"].as_array().unwrap().len(),
        3,
        "失败更新不落盘"
    );
}

// ========== M-4 T2 发现队列 + adopt 一键建模 ==========

/// 直推未建模样本进发现引擎（本套件无 pipeline——e2e 直调引擎面，
/// REST 面走 build_router；与 lib 单测不同测试进程，全局 ENGINE 不互扰）。
async fn discover(handle: &lux::ServerHandle, devid: &str, tag: &str, times: usize) {
    let client = handle.client();
    let effective = rs_iot::model::cache::effective();
    for i in 0..times {
        let mut values = std::collections::BTreeMap::new();
        values.insert(
            tag.to_string(),
            rs_iot::types::SampleValue::Float(i as f64 + 0.5),
        );
        let s = Sample {
            hwid: Hwid::new("it-hwid"),
            devid: devid.to_string(),
            values,
            timestamp: 1_790_000_000 + i as i64,
        };
        rs_iot::model::discovery::note_with_window(
            &client,
            &[s],
            &effective,
            std::time::Duration::ZERO,
            5000,
        )
        .await
        .expect("note");
    }
}

#[tokio::test]
async fn discoveries_api_requires_jwt() {
    let (app, handle) = test_app("m4-disc-401").await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/discoveries")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    drop(resp);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn discovery_list_sorted_by_count_and_filtered_by_dev() {
    let (app, handle) = test_app("m4-disc-list").await;
    let token = jwt(&app).await;
    discover(&handle, "ddA", "x", 1).await;
    discover(&handle, "ddB", "y", 3).await;

    let (st, v) = get_json(&app, &token, "/api/v1/discoveries").await;
    assert_eq!(st, StatusCode::OK, "{v:?}");
    let arr = v.as_array().expect("数组载荷（count 降序）");
    assert_eq!(arr.len(), 2, "{v:?}");
    assert_eq!(arr[0]["tag"], serde_json::json!("y"), "count 高在前");
    assert_eq!(arr[0]["count"], serde_json::json!(3));
    assert_eq!(arr[0]["devid"], serde_json::json!("ddB"));
    assert_eq!(arr[1]["count"], serde_json::json!(1));

    let (st, v) = get_json(&app, &token, "/api/v1/discoveries?dev_id=ddA").await;
    assert_eq!(st, StatusCode::OK);
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["tag"], serde_json::json!("x"));
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn discovery_clear_scoped_by_device_and_all() {
    let (app, handle) = test_app("m4-disc-clear").await;
    let token = jwt(&app).await;
    discover(&handle, "ddA", "x", 1).await;
    discover(&handle, "ddB", "y", 1).await;

    let (st, v) = post_json(
        &app,
        &token,
        "/api/v1/discovery/clear",
        serde_json::json!({"dev_id": "ddA"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v:?}");
    assert_eq!(v["cleared"], serde_json::json!(1));
    let (_, v) = get_json(&app, &token, "/api/v1/discoveries").await;
    assert_eq!(v.as_array().unwrap().len(), 1, "仅清指定设备");

    let (st, v) = post_json(
        &app,
        &token,
        "/api/v1/discovery/clear",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["cleared"], serde_json::json!(1));
    let (_, v) = get_json(&app, &token, "/api/v1/discoveries").await;
    assert!(v.as_array().unwrap().is_empty(), "全清");
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn adopt_merges_tags_sweeps_queue_and_gates_reflow() {
    let (app, handle) = test_app("m4-disc-adopt").await;
    let token = jwt(&app).await;
    // 模型 madopt（tag: ua）+ 设备 ddC。
    let (st, _) = post_json(
        &app,
        &token,
        "/api/v1/devmodel/set",
        serde_json::json!({
            "model_id": "madopt",
            "tags": [{"name": "ua", "data_type": "f32"}]
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = post_json(
        &app,
        &token,
        "/api/v1/devreg/set",
        serde_json::json!({"devid": "ddC", "model": "madopt"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // 未建模 tag 进队列。
    discover(&handle, "ddC", "ghost", 2).await;
    let (_, v) = get_json(&app, &token, "/api/v1/discoveries").await;
    assert_eq!(v.as_array().unwrap().len(), 1);

    // adopt：新 tag 并入模型 + 队列对账清除。
    let (st, v) = post_json(
        &app,
        &token,
        "/api/v1/discovery/adopt",
        serde_json::json!({
            "model_id": "madopt",
            "tags": [
                {"name": "ghost", "data_type": "f32", "unit": "V"},
                {"name": "ua", "data_type": "f32"}
            ]
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v:?}");
    assert_eq!(v["merged"], serde_json::json!(1), "同名 ua 跳过：{v:?}");
    assert_eq!(v["swept"], serde_json::json!(1), "队列对账清除：{v:?}");

    // 模型已含新 tag（含 unit 建议）。
    let (_, m) = get_json(&app, &token, "/api/v1/devmodels?model_id=madopt").await;
    let tags = m["madopt"]["tags"].as_array().unwrap();
    let ghost = tags
        .iter()
        .find(|t| t["name"] == "ghost")
        .expect("新 tag 入模型");
    assert_eq!(ghost["unit"], serde_json::json!("V"));

    // 再流同 tag 数据：已建模过闸（effective 缓存竞态下轮询重试——并行测试
    // 的 store 写会刷掉全局缓存，M-1 已知）。
    let client = handle.client();
    let mut gated = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        let eff = rs_iot::model::cache::reload(&client).await.unwrap();
        discover(&handle, "ddC", "ghost", 1).await;
        let (_, v) = get_json(&app, &token, "/api/v1/discoveries").await;
        if eff.contains_key("ddC.ghost") && v.as_array().unwrap().is_empty() {
            gated = true;
            break;
        }
        // 队列若被重新污染则清掉重试。
        let _ = post_json(
            &app,
            &token,
            "/api/v1/discovery/clear",
            serde_json::json!({}),
        )
        .await;
    }
    assert!(gated, "建模后同 tag 数据不再进队列（8s 内未达成）");
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn adopt_rejects_unknown_model_and_undeclared_param_addr() {
    let (app, handle) = test_app("m4-disc-adopt-bad").await;
    let token = jwt(&app).await;
    // 目标模型不存在 → 404。
    let (st, _) = post_json(
        &app,
        &token,
        "/api/v1/discovery/adopt",
        serde_json::json!({"model_id": "nope", "tags": [{"name": "x", "data_type": "f32"}]}),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // addr 引用未声明参数 → 400（结构校验）。
    let (st, _) = post_json(
        &app,
        &token,
        "/api/v1/devmodel/set",
        serde_json::json!({"model_id": "maddr", "tags": []}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, v) = post_json(
        &app,
        &token,
        "/api/v1/discovery/adopt",
        serde_json::json!({
            "model_id": "maddr",
            "tags": [{"name": "x", "data_type": "f32", "addr": "holding:{base}:f32"}]
        }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v:?}");
    common::stop_lux(handle).await;
}
