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
use rs_iot::config::HistoryConfig;
use rs_iot::history::engine::{maybe_init, HistoryEngine};
use rs_iot::history::{HistoryPoint, HistoryQuery, HistoryReader};
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::Hwid;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use tsink::{DataPoint, Label, Row, Value};

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
        rtdata_watch: std::sync::Arc::new(rs_iot::api::rtdata_watch::RtdataWatchService::new()),
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

    let cases: [(&str, &str); 13] = [
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
            "interval 无 agg",
            "/api/v1/history?dev_id=d&tag=t&start=0&end=1&interval=5",
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
            "interval 秒值溢出(u64::MAX)",
            "/api/v1/history?dev_id=d&tag=t&start=0&end=1&agg=avg&interval=18446744073709551615",
        ),
        (
            "降采样桶数超上限",
            "/api/v1/history?dev_id=d&tag=t&start=0&end=100001&agg=avg&interval=1",
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

// ---------- Task 2 红：raw select 读路径（真引擎——maybe_init 临时目录，
// storage() 直接 insert_rows 铺数据，reader("hw-test") 进 AppState） ----------

/// reader 锚定的进程 hwid：与铺数据 labels 的 hwid 同值——series 身份 =
/// metric + labels{devid, hwid}，reader 侧 hwid 错值 → labels 不匹配 →
/// 空结果（count 断言即抓）。
const HWID: &str = "hw-test";

/// 唯一临时目录（真引擎 data_dir）：tag + pid + 纳秒时钟，测试并行与 CI
/// 并发 run 不冲突；best-effort 清理责任在调用方（Windows 句柄释放有延迟）。
fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rsiot-history-read-e2e-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

/// 当前 Unix 秒（数据 ts 用 now 附近秒——retention 默认 30 天，勿用 2023
/// 固定值会被 ingest 侧 OutOfRetention 拒掉）。
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_secs() as i64
}

/// 真引擎构造：maybe_init(enabled + 临时 data_dir) → Some(Ok(engine))。
/// cardinality 显式 100——auto 校准对空 effective 兜底 1，容不下 4 series。
fn test_engine(tag: &str) -> (HistoryEngine, PathBuf) {
    let dir = unique_dir(tag);
    let cfg = HistoryConfig {
        enabled: true,
        data_dir: dir.to_string_lossy().into_owned(),
        cardinality_limit: 100,
        ..HistoryConfig::default()
    };
    let effective = HashMap::new();
    let engine = maybe_init(&cfg, &effective)
        .expect("enabled=true 必须 Some")
        .expect("临时目录上引擎初始化必须成功");
    (engine, dir)
}

/// 写路径 labels 契约 [devid, hwid]（字典序）：hwid 与 reader(HWID) 同值，
/// 锚定 reader 的 hwid 参数参与 series 身份。
fn labels(devid: &str) -> Vec<Label> {
    vec![Label::new("devid", devid), Label::new("hwid", HWID)]
}

/// 收尾三连：close 排空（15s 看门狗，释放 .tsink.lock）→ 停 lux → 清目录。
async fn teardown(engine: &mut HistoryEngine, handle: lux::ServerHandle, dir: &PathBuf) {
    engine.close(Duration::from_secs(15));
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(dir);
}

/// Task 2 红：raw 读回四型保真——真引擎写四 metric（f_t/f_i/f_b/f_s 同一
/// devid）各 5 点（ts 用 now 起始互异秒，ts_ms=sec*1000；值 Float 1.5 /
/// Int -7 / Bool true / Str "txt"）→ 四次 GET（无 agg）→ 各 count==5、
/// points[0].ts==写入首秒、值 JSON 类型精确（number/number/bool/string）
/// 且具体值保真。
#[tokio::test]
async fn history_raw_roundtrip_typed() {
    let (mut engine, dir) = test_engine("raw-rt");
    let t0 = now_secs();
    let cases: [(&str, Value); 4] = [
        ("f_t", Value::F64(1.5)),
        ("f_i", Value::I64(-7)),
        ("f_b", Value::Bool(true)),
        ("f_s", Value::String("txt".into())),
    ];
    let rows: Vec<Row> = cases
        .iter()
        .flat_map(|(m, v)| {
            (0..5)
                .map(|i| {
                    Row::with_labels(
                        *m,
                        labels("dev-rt"),
                        DataPoint::new((t0 + i) * 1000, v.clone()),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect();
    engine
        .storage()
        .insert_rows(&rows)
        .expect("铺数据 insert_rows 必须成功");

    let (app, handle) = test_app(Some(engine.reader(HWID)), Arc::from("ok")).await;
    let token = login_token(&app).await;

    for (metric, want) in &cases {
        let uri = format!(
            "/api/v1/history?dev_id=dev-rt&tag={metric}&start={t0}&end={}",
            t0 + 5
        );
        let resp = get_history(&app, &token, &uri).await;
        assert_eq!(resp.status(), StatusCode::OK, "{metric}: {uri}");
        let json = body_json(resp).await;
        assert_eq!(json["count"], 5, "{metric} 应恰回读 5 点");
        let points = json["points"].as_array().expect("points 应为数组");
        assert_eq!(points.len(), 5, "{metric} points 长度应与 count 一致");
        assert_eq!(points[0]["ts"], t0, "{metric} 首点 ts 应为写入首秒");
        let v0 = &points[0]["value"];
        // 类型精确：Float/Int → JSON number；Bool → bool；Str → string
        if matches!(want, Value::F64(_) | Value::I64(_)) {
            assert!(v0.is_number(), "{metric} 值应为 JSON number，实际: {v0}");
        } else if matches!(want, Value::Bool(_)) {
            assert!(v0.is_boolean(), "{metric} 值应为 JSON bool，实际: {v0}");
        } else {
            assert!(v0.is_string(), "{metric} 值应为 JSON string，实际: {v0}");
        }
        // 值保真（serde_json Number 跨 i64/f64 表示不等，一并锁数值精度类型）
        let want_json = match want {
            Value::F64(f) => serde_json::json!(f),
            Value::I64(i) => serde_json::json!(i),
            Value::Bool(b) => serde_json::json!(b),
            Value::String(s) => serde_json::json!(s),
            _ => unreachable!("铺数据仅四型，不可达"),
        };
        assert_eq!(v0, &want_json, "{metric} 值保真");
    }

    teardown(&mut engine, handle, &dir).await;
}

/// Task 2 红：半开窗 [start,end)——单 series 5 点 ts=t0..t0+4 →
/// GET start=t0&end=t0+2 → 恰 2 点（含 t0 不含 t0+2）。
#[tokio::test]
async fn history_raw_window_half_open() {
    let (mut engine, dir) = test_engine("raw-half");
    let t0 = now_secs();
    let rows: Vec<Row> = (0..5)
        .map(|i| {
            Row::with_labels(
                "w_m",
                labels("dev-w"),
                DataPoint::new((t0 + i) * 1000, Value::F64(i as f64)),
            )
        })
        .collect();
    engine
        .storage()
        .insert_rows(&rows)
        .expect("铺数据 insert_rows 必须成功");

    let (app, handle) = test_app(Some(engine.reader(HWID)), Arc::from("ok")).await;
    let token = login_token(&app).await;

    let uri = format!(
        "/api/v1/history?dev_id=dev-w&tag=w_m&start={t0}&end={}",
        t0 + 2
    );
    let resp = get_history(&app, &token, &uri).await;
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let json = body_json(resp).await;
    assert_eq!(json["count"], 2, "半开窗 [t0,t0+2) 应恰 2 点");
    let ts: Vec<i64> = json["points"]
        .as_array()
        .expect("points 数组")
        .iter()
        .map(|p| p["ts"].as_i64().expect("ts 应为整数"))
        .collect();
    assert_eq!(ts, vec![t0, t0 + 1], "含 t0 不含 t0+2（半开窗语义）");

    teardown(&mut engine, handle, &dir).await;
}

/// Task 2 红：limit 取最旧——10 点 ts 递增（t0..t0+9）→ limit=3 →
/// 最旧 3 点升序（ts 严格递增、首点==t0；tsink 语义 limit 取时间升序
/// 最旧 n，非最新）。
#[tokio::test]
async fn history_raw_limit_oldest_first() {
    let (mut engine, dir) = test_engine("raw-limit");
    let t0 = now_secs();
    let rows: Vec<Row> = (0..10)
        .map(|i| {
            Row::with_labels(
                "l_m",
                labels("dev-l"),
                DataPoint::new((t0 + i) * 1000, Value::F64(i as f64)),
            )
        })
        .collect();
    engine
        .storage()
        .insert_rows(&rows)
        .expect("铺数据 insert_rows 必须成功");

    let (app, handle) = test_app(Some(engine.reader(HWID)), Arc::from("ok")).await;
    let token = login_token(&app).await;

    let uri = format!(
        "/api/v1/history?dev_id=dev-l&tag=l_m&start={t0}&end={}&limit=3",
        t0 + 10
    );
    let resp = get_history(&app, &token, &uri).await;
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let json = body_json(resp).await;
    assert_eq!(json["count"], 3, "limit=3 应恰返回 3 点");
    let ts: Vec<i64> = json["points"]
        .as_array()
        .expect("points 数组")
        .iter()
        .map(|p| p["ts"].as_i64().expect("ts 应为整数"))
        .collect();
    assert_eq!(ts.first(), Some(&t0), "limit 取最旧：首点应为 t0");
    assert!(
        ts.windows(2).all(|w| w[0] < w[1]),
        "ts 须严格递增，实际: {ts:?}"
    );
    assert_eq!(ts, vec![t0, t0 + 1, t0 + 2], "应恰为最旧 3 点升序");

    teardown(&mut engine, handle, &dir).await;
}

/// Task 2 红：未写过的 (dev,tag) → 200 `{points: [], count: 0}`（空窗/
/// 不存在 series 是 Ok 而非错误——tsink 语义）。T1 的 shape 测走
/// FakeReader，本测锁定**真引擎**路径（占位 select 恒 Err → 本测当前
/// 500，随 Task 2 绿转 200）。
#[tokio::test]
async fn history_raw_missing_series_200_empty() {
    let (mut engine, dir) = test_engine("raw-miss");
    let (app, handle) = test_app(Some(engine.reader(HWID)), Arc::from("ok")).await;
    let token = login_token(&app).await;

    let t = now_secs();
    let uri = format!(
        "/api/v1/history?dev_id=dev-none&tag=never&start={t}&end={}",
        t + 60
    );
    let resp = get_history(&app, &token, &uri).await;
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let json = body_json(resp).await;
    assert_eq!(json["points"], serde_json::json!([]));
    assert_eq!(json["count"], 0);

    teardown(&mut engine, handle, &dir).await;
}

// ---------- Task 3 红：downsample 六聚合 + 类型矩阵 + 400 映射 ----------
// 现状：engine select 的 downsample 分支占位 Err（"downsample 待 Task 3
// 实现"）→ routes Err 统一 500，下列各测红在 status 500；其中 Bool+avg
// 红在 500≠400（P12 要求 UnsupportedAggregation→400，当前 REST 层未分层）。

/// Task 3 红：downsample 桶语义（P10）——单 series 10 点（ts=t0..t0+9
/// 每秒，v=1..=10 Float）→ `interval=5` 查询（start=t0&end=t0+10）→
/// 六聚合各恰 2 桶：ts==t0 与 t0+5（桶 ts=桶起点，**origin=查询 start
/// 即 t0**——非 epoch 对齐）；桶值为桶内点集的聚合（一测内多次 GET）。
/// 值矩阵（Float series）：sum/last/min/max/avg → F64 number（sum 保
/// 数值域、avg 恒 F64）；count → 整 number（Count 恒 U64）。
#[tokio::test]
async fn history_downsample_buckets() {
    let (mut engine, dir) = test_engine("ds-bucket");
    let t0 = now_secs();
    let rows: Vec<Row> = (0..10)
        .map(|i| {
            Row::with_labels(
                "ds_m",
                labels("dev-ds"),
                DataPoint::new((t0 + i) * 1000, Value::F64((i + 1) as f64)),
            )
        })
        .collect();
    engine
        .storage()
        .insert_rows(&rows)
        .expect("铺数据 insert_rows 必须成功");

    let (app, handle) = test_app(Some(engine.reader(HWID)), Arc::from("ok")).await;
    let token = login_token(&app).await;

    // (agg, [桶 t0 值, 桶 t0+5 值])：桶 t0 含 v1..v5，桶 t0+5 含 v6..v10。
    // sum 第二桶 40.0（6+7+8+9+10）——任务原文/计划文档写 35 系算术
    // 笔误：与本测自身 avg(8.0)×count(5)=40 及数据铺设互斥，任何实现
    // 无法满足；按冻结语义纠正为 40.0（同 ② Bool+last 的纠正先例），
    // 见 docs/decisions.md 与绿阶段回传报告。
    let cases: [(&str, serde_json::Value); 6] = [
        ("sum", serde_json::json!([15.0, 40.0])),
        ("last", serde_json::json!([5.0, 10.0])),
        ("min", serde_json::json!([1.0, 6.0])),
        ("max", serde_json::json!([5.0, 10.0])),
        ("count", serde_json::json!([5, 5])),
        ("avg", serde_json::json!([3.0, 8.0])),
    ];
    for (agg, want_vals) in &cases {
        let uri = format!(
            "/api/v1/history?dev_id=dev-ds&tag=ds_m&start={t0}&end={}&interval=5&agg={agg}",
            t0 + 10
        );
        let resp = get_history(&app, &token, &uri).await;
        assert_eq!(resp.status(), StatusCode::OK, "{agg}: {uri}");
        let json = body_json(resp).await;
        assert_eq!(json["count"], 2, "{agg} interval=5 覆盖 10 点应恰 2 桶");
        let points = json["points"].as_array().expect("points 应为数组");
        let ts: Vec<i64> = points
            .iter()
            .map(|p| p["ts"].as_i64().expect("ts 应为整数"))
            .collect();
        // P10：桶 ts=桶起点 + origin=查询 start（t0）→ 恰 [t0, t0+5]，
        // 且 count 的整数 JSON（U64）与其余 F64 一并由值比较锁定。
        assert_eq!(ts, vec![t0, t0 + 5], "{agg} 桶起点应 origin=查询 start");
        let vals: serde_json::Value = points.iter().map(|p| p["value"].clone()).collect();
        assert_eq!(vals, *want_vals, "{agg} 桶值");
    }

    teardown(&mut engine, handle, &dir).await;
}

/// Task 3 红：聚合类型退化矩阵 + UnsupportedAggregation→400（P12）。
/// 同 devid 三 series：i_cnt（I64 1,2,3）/ b_last（Bool true,false,true）/
/// s_min（Str "a","b"），interval=5 单桶覆盖全点：
/// ① i_cnt agg=count → number 且 ==3（Count 恒 U64，整型 JSON）；
/// ② b_last agg=last → JSON boolean（Last 保原类型；ts 升序末点值
///    true——任务原文写 false 与 tsink `Last => points.last()` 语义
///    冲突，按冻结语义断言 true，见会话回传报告）；
/// ③ b_last agg=avg → **400**（数值聚合作用于 Bool → tsink
///    UnsupportedAggregation，REST 须映射 400；当前占位 Err 统一 500，
///    本测红在 500≠400）；
/// ④ s_min agg=min → JSON string "a"（Min 保原类型）。
#[tokio::test]
async fn history_agg_type_matrix() {
    let (mut engine, dir) = test_engine("agg-matrix");
    let t0 = now_secs();
    let series: [(&str, Vec<Value>); 3] = [
        ("i_cnt", vec![Value::I64(1), Value::I64(2), Value::I64(3)]),
        (
            "b_last",
            vec![Value::Bool(true), Value::Bool(false), Value::Bool(true)],
        ),
        (
            "s_min",
            vec![Value::String("a".into()), Value::String("b".into())],
        ),
    ];
    let rows: Vec<Row> = series
        .into_iter()
        .flat_map(|(m, vs)| {
            vs.iter()
                .enumerate()
                .map(|(i, v)| {
                    Row::with_labels(
                        m,
                        labels("dev-mx"),
                        DataPoint::new((t0 + i as i64) * 1000, v.clone()),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect();
    engine
        .storage()
        .insert_rows(&rows)
        .expect("铺数据 insert_rows 必须成功");

    let (app, handle) = test_app(Some(engine.reader(HWID)), Arc::from("ok")).await;
    let token = login_token(&app).await;

    // 单桶取值 helper：GET（interval=5 覆盖全点）→ 200 + 恰 1 桶 +
    // points[0]["value"]。
    async fn bucket_value(
        app: &axum::Router,
        token: &str,
        tag: &str,
        agg: &str,
        start: i64,
        end: i64,
    ) -> serde_json::Value {
        let uri = format!(
            "/api/v1/history?dev_id=dev-mx&tag={tag}&start={start}&end={end}&interval=5&agg={agg}"
        );
        let resp = get_history(app, token, &uri).await;
        assert_eq!(resp.status(), StatusCode::OK, "{tag} {agg}: {uri}");
        let json = body_json(resp).await;
        assert_eq!(json["count"], 1, "{tag} {agg} interval=5 应单桶");
        json["points"][0]["value"].clone()
    }

    // ① I64 count → number 且 ==3（as_i64 同时锁整型 JSON 表示）
    let v = bucket_value(&app, &token, "i_cnt", "count", t0, t0 + 3).await;
    assert!(v.is_number(), "I64 count 应为 JSON number，实际: {v}");
    assert_eq!(v.as_i64(), Some(3), "I64 3 点 count 应 ==3");

    // ② Bool last → JSON boolean，值 true（ts 升序末点）
    let v = bucket_value(&app, &token, "b_last", "last", t0, t0 + 3).await;
    assert!(v.is_boolean(), "Bool last 应为 JSON boolean，实际: {v}");
    assert_eq!(v, serde_json::json!(true), "Bool last 应取时间末点值 true");

    // ③ Bool avg → 400（UnsupportedAggregation 的 P12 映射；当前 500 红）
    let uri = format!(
        "/api/v1/history?dev_id=dev-mx&tag=b_last&start={t0}&end={}&interval=5&agg=avg",
        t0 + 3
    );
    let resp = get_history(&app, &token, &uri).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "Bool 上数值聚合 UnsupportedAggregation 须 400（P12），uri: {uri}"
    );

    // ④ Str min → JSON string "a"（保原类型，字典序最小）
    let v = bucket_value(&app, &token, "s_min", "min", t0, t0 + 2).await;
    assert!(v.is_string(), "Str min 应为 JSON string，实际: {v}");
    assert_eq!(v, serde_json::json!("a"), "Str min 保原串");

    teardown(&mut engine, handle, &dir).await;
}

/// Task 3 红：空桶省略（P10 无 gap filling）——5 点 ts=t0,t0+1,t0+2,
/// t0+20,t0+21（interval=5，origin=查询 start=t0）→ 仅桶 t0（3 点）与
/// t0+20（2 点）输出，中间 t0+5/t0+10/t0+15 空桶不出现；count 聚合的
/// 桶值 3/2 同时锚定各桶点归属。
#[tokio::test]
async fn history_downsample_empty_bucket_skipped() {
    let (mut engine, dir) = test_engine("ds-gap");
    let t0 = now_secs();
    let offsets = [0i64, 1, 2, 20, 21];
    let rows: Vec<Row> = offsets
        .iter()
        .enumerate()
        .map(|(i, off)| {
            Row::with_labels(
                "gap_m",
                labels("dev-gap"),
                DataPoint::new((t0 + off) * 1000, Value::F64((i + 1) as f64)),
            )
        })
        .collect();
    engine
        .storage()
        .insert_rows(&rows)
        .expect("铺数据 insert_rows 必须成功");

    let (app, handle) = test_app(Some(engine.reader(HWID)), Arc::from("ok")).await;
    let token = login_token(&app).await;

    let uri = format!(
        "/api/v1/history?dev_id=dev-gap&tag=gap_m&start={t0}&end={}&interval=5&agg=count",
        t0 + 22
    );
    let resp = get_history(&app, &token, &uri).await;
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let json = body_json(resp).await;
    assert_eq!(json["count"], 2, "仅两非空桶，中间空桶不输出");
    let points = json["points"].as_array().expect("points 应为数组");
    let ts: Vec<i64> = points
        .iter()
        .map(|p| p["ts"].as_i64().expect("ts 应为整数"))
        .collect();
    assert_eq!(
        ts,
        vec![t0, t0 + 20],
        "空桶 t0+5/t0+10/t0+15 不得补点（P10 无 gap filling）"
    );
    let vals: serde_json::Value = points.iter().map(|p| p["value"].clone()).collect();
    assert_eq!(
        vals,
        serde_json::json!([3, 2]),
        "桶点归属：t0 桶 3 点、t0+20 桶 2 点"
    );

    teardown(&mut engine, handle, &dir).await;
}
