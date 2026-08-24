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
/// pipeline）。返回 (router, lux handle)。
async fn test_app(name: &str) -> (axum::Router, lux::ServerHandle) {
    let data_dir = common::test_data_dir(name);
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
