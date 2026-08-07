//! M7 Lua REST 端点集成测：stats 读 stat / reload 404 / scripts 列表形状 / JWT 保护。
//! spec §13 + §4。仿 api_flow.rs（router oneshot + JWT token）。

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::db::luastat::LuaStat;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::Hwid;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 构造带（可选）预写 stat 的 app。
async fn app_with_stat(stat_inst: &str) -> (axum::Router, lux::ServerHandle) {
    let data_dir = common::test_data_dir("lua-api");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");
    if !stat_inst.is_empty() {
        let stat = LuaStat {
            status: "running".into(),
            collect_count: 3,
            collect_fail_count: 0,
            last_collect_ts: 99,
            last_collect_duration_ms: 5,
            vm_restart_count: 1,
            script_version: "deadbeef".into(),
            last_err: String::new(),
        };
        rs_iot::db::luastat::write(&client, stat_inst, &stat)
            .await
            .expect("write stat");
    }
    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown,
        ctl_registry: CtlRegistry::new(),
        bridge_registry: BridgeRegistry::new(),
    };
    let (hub_tx, _hub_rx) = tokio::sync::mpsc::channel(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));
    let state = AppState {
        lux: client.clone(),
        mgr,
        hwid: Hwid::new("it-hwid"),
        jwt_secret: "test-secret-0123456789abcdef".into(),
        version: "itest",
    };
    (build_router(state), handle)
}

async fn login_token(app: &axum::Router) -> String {
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
    serde_json::from_slice::<serde_json::Value>(&bytes).expect("json")["token"]
        .as_str()
        .expect("token")
        .to_string()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

#[tokio::test]
async fn stats_returns_written_stat() {
    let (app, handle) = app_with_stat("lua-api").await;
    let token = login_token(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/lua/stats?inst_id=lua-api")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "running");
    assert_eq!(json["collect_count"], 3);
    assert_eq!(json["script_version"], "deadbeef");
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn stats_missing_returns_empty_object() {
    let (app, handle) = app_with_stat("").await;
    let token = login_token(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/lua/stats?inst_id=nonexist")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(
        json.as_object().is_some_and(|m| m.is_empty()),
        "无 stat 返回空对象 {{}}，得到 {json}"
    );
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn stats_requires_token() {
    let (app, handle) = app_with_stat("").await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/lua/stats?inst_id=x")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn reload_unknown_instance_404() {
    let (app, handle) = app_with_stat("").await;
    let token = login_token(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lua/reload?inst_id=nonexist")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn scripts_returns_list_shape() {
    let (app, handle) = app_with_stat("").await;
    let token = login_token(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/lua/scripts")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("call");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["scripts"].is_array(), "返回 {{ scripts: [...] }}");
    common::stop_lux(handle).await;
}
