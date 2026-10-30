//! BI 大屏数据端点 e2e（P1，2026-10-29）：
//! - `GET /api/v1/history/stats`：单 series 整窗统计（count/last/avg/min/max/sum）
//!   ——KPI 卡一 call 一卡，免前端拉全量点自算；
//! - `POST /api/v1/history/batch`：多 series 批量查询（并发 cap 4，单条错误
//!   字段化不拖整批）——一屏多图一次往返。
//!
//! 行为矩阵对齐既有 `/api/v1/history`：503（reader 未就绪按 engine_state 分
//! reason）、400 参数错在解析层拦截（ScriptedReader 计数证零调用）、秒出入。

#![cfg(feature = "history")]

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::history::{HistoryPoint, HistoryQuery, HistoryReader};
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{Hwid, SampleValue};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 脚本 reader：按 "dev|tag" 键返回固定点集，并计数 select 调用。
struct ScriptedReader {
    points: HashMap<String, Vec<(i64, f64)>>,
    calls: AtomicUsize,
}

impl ScriptedReader {
    fn new() -> Arc<Self> {
        // d1.t1：ts 1000..6000ms 步 1000，值 10..60（avg=35 min=10 max=60 sum=210）
        Arc::new(ScriptedReader {
            points: HashMap::from([(
                "d1|t1".to_string(),
                (0..6)
                    .map(|i| (1000 + i * 1000, 10.0 + i as f64 * 10.0))
                    .collect(),
            )]),
            calls: AtomicUsize::new(0),
        })
    }
}

impl HistoryReader for ScriptedReader {
    fn select(
        &self,
        metric: &str,
        devid: &str,
        q: &HistoryQuery,
    ) -> Result<Vec<HistoryPoint>, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let pts = self
            .points
            .get(&format!("{devid}|{metric}"))
            .cloned()
            .unwrap_or_default();
        let win: Vec<_> = pts
            .into_iter()
            .filter(|(ts, _)| *ts >= q.start_ms && *ts < q.end_ms)
            .collect();
        // 降采样模拟（avg）：按桶宽分桶取均值，桶 ts=桶内首点 ts（够用桩）
        if let Some((bucket_ms, rs_iot::history::HistoryAgg::Avg)) = q.downsample {
            let mut buckets: Vec<(i64, Vec<f64>)> = vec![];
            for (ts, v) in win {
                let key = ts / bucket_ms * bucket_ms;
                match buckets.last_mut() {
                    Some((k, vs)) if *k == key => vs.push(v),
                    _ => buckets.push((key, vec![v])),
                }
            }
            return Ok(buckets
                .into_iter()
                .map(|(k, vs)| HistoryPoint {
                    ts_ms: k,
                    value: SampleValue::Float(vs.iter().sum::<f64>() / vs.len() as f64),
                })
                .collect());
        }
        Ok(win
            .into_iter()
            .map(|(ts, v)| HistoryPoint {
                ts_ms: ts,
                value: SampleValue::Float(v),
            })
            .collect())
    }
}

async fn test_app(
    history_reader: Option<Arc<dyn HistoryReader>>,
) -> (axum::Router, lux::ServerHandle) {
    let data_dir = common::test_data_dir("hist_bi");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");
    let state_label = if history_reader.is_some() {
        "ok"
    } else {
        "disabled"
    };
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(Arc::new(rs_iot::process_metrics::ProcessMetrics::new())),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
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
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        log_ringbuffer: Arc::new(rs_iot::log_ringbuffer::LogRingBuffer::new(2000)),
        event_bus: rs_iot::event_bus::EventBus::dummy(),
        config: Arc::new(rs_iot::config::AppConfig::default()),
        history_stats: None,
        history_engine_state: Arc::from(state_label),
        history_reader,
        history_telemetry: None,
        rtdata_watch: Arc::new(rs_iot::api::rtdata_watch::RtdataWatchService::new()),
    };
    (build_router(state), handle)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("json body")
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
                .expect("request"),
        )
        .await
        .expect("call app");
    body_json(resp).await["token"]
        .as_str()
        .expect("token")
        .to_string()
}

async fn req(
    app: &axum::Router,
    token: &str,
    method: &str,
    uri: &str,
    body: &str,
) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, token)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("call app")
}

// ── stats ───────────────────────────────────────────────────────────

/// 整窗统计：六值一次返回（count/last/avg/min/max/sum）+ last_ts。
#[tokio::test]
async fn stats_aggregates_full_window() {
    let (app, handle) = test_app(Some(ScriptedReader::new())).await;
    let token = login_token(&app).await;
    // 秒出入：start=0 end=10 覆盖全部 6 点（ms 1000..6000）
    let resp = req(
        &app,
        &token,
        "GET",
        "/api/v1/history/stats?dev_id=d1&tag=t1&start=0&end=10",
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "stats 200");
    let json = body_json(resp).await;
    assert_eq!(json["dev_id"], "d1");
    assert_eq!(json["tag"], "t1");
    assert_eq!(json["stats"]["count"], 6);
    assert_eq!(json["stats"]["sum"], 210.0);
    assert_eq!(json["stats"]["avg"], 35.0);
    assert_eq!(json["stats"]["min"], 10.0);
    assert_eq!(json["stats"]["max"], 60.0);
    assert_eq!(json["stats"]["last"], 60.0);
    assert_eq!(json["stats"]["last_ts"], 6, "last_ts 秒基线（6000ms/1000）");
    assert_eq!(json["stats"]["truncated"], false, "未触上限不标截断");
    common::stop_lux(handle).await;
}

/// 截断标志（2026-10-30 审查修复）：整窗点数 > raw 上限（MAX_LIMIT=100000）时
/// stats 基于最旧 100k 点计算，sum/last/last_ts 会静默失真——响应必须带
/// `truncated:true`；恰好 100k 无法区分真值与截断，保守置 true。
#[tokio::test]
async fn stats_marks_truncation_at_raw_read_limit() {
    struct LimitReader {
        n: usize,
    }
    impl HistoryReader for LimitReader {
        fn select(
            &self,
            _metric: &str,
            _devid: &str,
            _q: &HistoryQuery,
        ) -> Result<Vec<HistoryPoint>, String> {
            Ok((0..self.n)
                .map(|i| HistoryPoint {
                    ts_ms: 1000 + i as i64,
                    value: SampleValue::Float(i as f64),
                })
                .collect())
        }
    }

    let (app, handle) = test_app(Some(Arc::new(LimitReader { n: 100_000 }))).await;
    let token = login_token(&app).await;
    let resp = req(
        &app,
        &token,
        "GET",
        "/api/v1/history/stats?dev_id=d1&tag=t1&start=0&end=200000",
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["stats"]["count"], 100_000);
    assert_eq!(
        json["stats"]["truncated"], true,
        "命中 raw 读取上限必须标记截断（KPI sum/last 基于最旧 100k 点）"
    );
    common::stop_lux(handle).await;

    let (app, handle) = test_app(Some(Arc::new(LimitReader { n: 99_999 }))).await;
    let token = login_token(&app).await;
    let resp = req(
        &app,
        &token,
        "GET",
        "/api/v1/history/stats?dev_id=d1&tag=t1&start=0&end=200000",
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["stats"]["count"], 99_999);
    assert_eq!(json["stats"]["truncated"], false, "低于上限不标截断");
    common::stop_lux(handle).await;
}

/// 空窗：count=0、数值字段 null（前端 KPI 显示占位）。
#[tokio::test]
async fn stats_empty_window_is_count_zero_with_nulls() {
    let (app, handle) = test_app(Some(ScriptedReader::new())).await;
    let token = login_token(&app).await;
    let resp = req(
        &app,
        &token,
        "GET",
        "/api/v1/history/stats?dev_id=d1&tag=t1&start=100&end=200",
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["stats"]["count"], 0);
    assert!(json["stats"]["avg"].is_null(), "空窗数值聚合为 null");
    assert!(json["stats"]["last"].is_null());
    common::stop_lux(handle).await;
}

/// 400：必填缺失 / start>=end 在解析层拦截（select 零调用）。
#[tokio::test]
async fn stats_400_matrix_without_touching_reader() {
    let reader = ScriptedReader::new();
    let weak = Arc::downgrade(&reader);
    let (app, handle) = test_app(Some(reader)).await;
    let token = login_token(&app).await;
    for uri in [
        "/api/v1/history/stats?tag=t1&start=0&end=10",
        "/api/v1/history/stats?dev_id=d1&tag=t1&start=10&end=10",
    ] {
        let resp = req(&app, &token, "GET", uri, "").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "400: {uri}");
    }
    let reader = weak.upgrade().expect("reader alive");
    assert_eq!(
        reader.calls.load(Ordering::Relaxed),
        0,
        "参数错不得打到 reader"
    );
    common::stop_lux(handle).await;
}

/// 503：reader 未就绪 → 与 /history 同款 reason 矩阵。
#[tokio::test]
async fn stats_503_reason_matrix() {
    let (app, handle) = test_app(None).await;
    let token = login_token(&app).await;
    let resp = req(
        &app,
        &token,
        "GET",
        "/api/v1/history/stats?dev_id=d1&tag=t1&start=0&end=10",
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(resp).await;
    assert_eq!(json["reason"], "history_disabled");
    common::stop_lux(handle).await;
}

// ── batch ───────────────────────────────────────────────────────────

const BATCH_BODY: &str = r#"{"queries":[
  {"dev_id":"d1","tag":"t1","start":0,"end":10},
  {"dev_id":"d1","tag":"t1","start":0,"end":10,"agg":"avg","interval":5}
]}"#;

/// 多查询一次往返：results 顺序对应 queries，各自带 points/count。
#[tokio::test]
async fn batch_returns_results_per_query_in_order() {
    let (app, handle) = test_app(Some(ScriptedReader::new())).await;
    let token = login_token(&app).await;
    let resp = req(&app, &token, "POST", "/api/v1/history/batch", BATCH_BODY).await;
    assert_eq!(resp.status(), StatusCode::OK, "batch 200");
    let json = body_json(resp).await;
    let results = json["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2, "结果数与查询数一致");
    assert_eq!(results[0]["count"], 6, "raw 全窗 6 点");
    assert_eq!(results[1]["count"], 2, "降采样 5s 桶 2 个");
    assert_eq!(results[0]["points"][0]["ts"], 1, "ts 秒基线");
    common::stop_lux(handle).await;
}

/// 单条参数错字段化：该条 {error}、其余正常、整体 200（单条失败不拖全屏）。
#[tokio::test]
async fn batch_single_bad_query_is_field_error_not_whole_400() {
    let body = r#"{"queries":[
      {"dev_id":"d1","tag":"t1","start":0,"end":10},
      {"dev_id":"d1","tag":"t1","start":10,"end":10}
    ]}"#;
    let (app, handle) = test_app(Some(ScriptedReader::new())).await;
    let token = login_token(&app).await;
    let resp = req(&app, &token, "POST", "/api/v1/history/batch", body).await;
    assert_eq!(resp.status(), StatusCode::OK, "整体 200");
    let json = body_json(resp).await;
    let results = json["results"].as_array().expect("results");
    assert_eq!(results[0]["count"], 6, "正常条照常返回");
    assert!(
        results[1].get("error").is_some(),
        "坏条字段化 error：{}",
        results[1]
    );
    assert!(results[1].get("points").is_none(), "坏条不带 points");
    common::stop_lux(handle).await;
}

/// 批级 400：空 queries / 超 32 条上限。
#[tokio::test]
async fn batch_400_on_empty_or_over_limit() {
    let (app, handle) = test_app(Some(ScriptedReader::new())).await;
    let token = login_token(&app).await;
    let resp = req(
        &app,
        &token,
        "POST",
        "/api/v1/history/batch",
        r#"{"queries":[]}"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "空 queries 400");
    let over: Vec<String> = (0..33)
        .map(|i| format!(r#"{{"dev_id":"d{i}","tag":"t","start":0,"end":10}}"#))
        .collect();
    let body = format!(r#"{{"queries":[{}]}}"#, over.join(","));
    let resp = req(&app, &token, "POST", "/api/v1/history/batch", &body).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "33 条超限 400");
    common::stop_lux(handle).await;
}
