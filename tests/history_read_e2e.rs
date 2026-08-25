//! 历史读端点 e2e（Phase 2 Task 1 红）：`GET /api/v1/history`（JWT 保护）的
//! 503 三态矩阵（P5）、400 参数矩阵（P3/P12，解析层拦截不打到 reader）与
//! reader 就绪态空结果的 200 响应形 `{dev_id, tag, points, count}`。
//! 设计与裁定见 docs/designs/2026-08-25-history-read-subscribe-plan.md。
//!
//! feature on 编译下 `feature_not_compiled` 分支不可达（cfg! 编译期求值），
//! 503 矩阵只断言 disabled/init_failed 两态。

#![cfg(feature = "history")]

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::history::{HistoryPoint, HistoryQuery, HistoryReader};
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::Hwid;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 假 reader：恒返回空结果并计数 select 调用次数——400 矩阵据此断言参数错
/// 在解析层拦截、不打到 reader。
struct FakeReader {
    calls: AtomicUsize,
}

impl FakeReader {
    fn new() -> Arc<Self> {
        Arc::new(FakeReader {
            calls: AtomicUsize::new(0),
        })
    }
}

impl HistoryReader for FakeReader {
    fn select(
        &self,
        _metric: &str,
        _devid: &str,
        _q: &HistoryQuery,
    ) -> Result<Vec<HistoryPoint>, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(vec![])
    }
}

/// 构造注入指定 history reader 测试态的 axum app（其余照
/// history_visibility_e2e 范本）。
async fn test_app(
    history_reader: Option<Arc<dyn HistoryReader>>,
    history_engine_state: Arc<str>,
) -> (axum::Router, lux::ServerHandle) {
    let data_dir = common::test_data_dir("hist_read");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown,
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
        history: None,
    };
    let (hub_tx, _hub_rx) = tokio::sync::mpsc::channel(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));
    let state = AppState {
        lux: client.clone(),
        mgr,
        hwid: Hwid::new("it-hwid"),
        jwt_secret: "test-secret-0123456789abcdef".into(),
        version: "itest",
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        log_ringbuffer: std::sync::Arc::new(rs_iot::log_ringbuffer::LogRingBuffer::new(2000)),
        event_bus: rs_iot::event_bus::EventBus::dummy(),
        config: std::sync::Arc::new(rs_iot::config::AppConfig::default()),
        history_stats: None,
        history_engine_state,
        history_reader,
    };
    (build_router(state), handle)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("json body")
}

/// 登录拿 JWT（/api/v1/history 受保护，照 api_flow 范本）。
async fn login_token(app: &axum::Router) -> String {
    let resp = app
        .clone()
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

/// 带 JWT GET 一次 history 端点。
async fn get_history(app: &axum::Router, token: &str, uri: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header(header::AUTHORIZATION, token)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app")
}

/// 503 三态矩阵（P5）：reader=None + engine 非 ok → 503，body
/// `{error:"history unavailable", reason}`，reason 随 engine_state 区分
/// disabled / init_failed（feature on 下 feature_not_compiled 不可达，不测）。
#[tokio::test]
async fn history_503_matrix() {
    // engine="disabled" → reason="history_disabled"
    let (app, handle) = test_app(None, Arc::from("disabled")).await;
    let token = login_token(&app).await;
    let resp = get_history(&app, &token, "/api/v1/history?dev_id=d&tag=t&start=0&end=1").await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(resp).await;
    assert_eq!(json["error"], "history unavailable");
    assert_eq!(json["reason"], "history_disabled");
    common::stop_lux(handle).await;

    // engine="init_failed" → reason="init_failed"
    let (app, handle) = test_app(None, Arc::from("init_failed")).await;
    let token = login_token(&app).await;
    let resp = get_history(&app, &token, "/api/v1/history?dev_id=d&tag=t&start=0&end=1").await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(resp).await;
    assert_eq!(json["error"], "history unavailable");
    assert_eq!(json["reason"], "init_failed");
    common::stop_lux(handle).await;
}

/// 400 参数矩阵（P3/P12）：reader 就绪（FakeReader）+ engine="ok"，各参数错
/// → 400；全程 select 零调用——参数错在解析层拦截，不打到 reader。
#[tokio::test]
async fn history_400_param_matrix() {
    let reader = FakeReader::new();
    let (app, handle) = test_app(Some(reader.clone()), Arc::from("ok")).await;
    let token = login_token(&app).await;

    let cases: [(&str, &str); 10] = [
        ("缺 dev_id", "/api/v1/history?tag=t&start=0&end=1"),
        ("缺 tag", "/api/v1/history?dev_id=d&start=0&end=1"),
        ("缺 start", "/api/v1/history?dev_id=d&tag=t&end=1"),
        ("缺 end", "/api/v1/history?dev_id=d&tag=t&start=0"),
        ("start==end", "/api/v1/history?dev_id=d&tag=t&start=5&end=5"),
        ("start>end", "/api/v1/history?dev_id=d&tag=t&start=6&end=5"),
        (
            "agg 无 interval",
            "/api/v1/history?dev_id=d&tag=t&start=0&end=1&agg=avg",
        ),
        (
            "agg 未知值",
            "/api/v1/history?dev_id=d&tag=t&start=0&end=1&agg=bogus&interval=5",
        ),
        (
            "interval=0",
            "/api/v1/history?dev_id=d&tag=t&start=0&end=1&agg=avg&interval=0",
        ),
        (
            "limit 超上限",
            "/api/v1/history?dev_id=d&tag=t&start=0&end=1&limit=200000",
        ),
    ];
    for (name, uri) in cases {
        let resp = get_history(&app, &token, uri).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{name}: {uri}");
    }

    assert_eq!(
        reader.calls.load(Ordering::Relaxed),
        0,
        "参数错须在解析层拦截，不得打到 reader"
    );
    common::stop_lux(handle).await;
}

/// reader 就绪态 + 空 series：200 `{dev_id, tag, points: [], count: 0}`
/// （空窗/不存在 series 是 Ok(vec![]) 而非错误——tsink 语义）。
#[tokio::test]
async fn history_ok_shape_empty() {
    let (app, handle) = test_app(Some(FakeReader::new()), Arc::from("ok")).await;
    let token = login_token(&app).await;

    let resp = get_history(&app, &token, "/api/v1/history?dev_id=d&tag=t&start=0&end=1").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["dev_id"], "d");
    assert_eq!(json["tag"], "t");
    assert_eq!(json["points"], serde_json::json!([]));
    assert_eq!(json["count"], 0);

    common::stop_lux(handle).await;
}
