//! 历史可见性薄切片 e2e（Phase 3 前置）：/sysstatus history 块 + /metrics
//! `rs_iot_history_*_total` 7 family。AppState 注入测试态（stats 预置非零值 /
//! engine_state），断言两出口的精确输出与省略语义（引擎非 ok 不输出）。
//!
//! Phase 3 治理扩展（Task 3 红）：FakeTelemetry 注入 + preset 扩新计数器 →
//! /metrics 治理七 family（水位/series/archived_tags/死区评估与削减率/写错误
//! 五桶）与 /sysstatus history 块 P6 新字段的精确输出与省略语义。

#![cfg(feature = "history")]

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rs_iot::api::{build_router, AppState};
use rs_iot::history::rows::HistoryStats;
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{ChildSpec, DeviceSpec, Hwid, TagProps};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 预置非零七计数器（每计数器取互异值，精确断言防串位）。
///
/// Phase 3 治理扩展（P2/P3 口径）：`deadband_evaluated=8` → 削减率
/// ratio = deadbanded/evaluated = 5/8 = 0.625（恰为 f64 精确表示，断言可写
/// 死 "0.625"）；`error_rows` 五桶（下标序 = `WriteErrorKind::ALL`：
/// memory_budget/cardinality/wal_disk/closed/other）取 3/0/1/0/5，
/// Σ=9 与 `errors` 现值一致（`record_error_rows` 单入口不变量，preset 直写
/// 原子槽须人工对齐——既有两测的 errors==9 断言保持不动）。
fn preset_stats() -> Arc<HistoryStats> {
    let s = HistoryStats::default();
    s.sent.store(11, Ordering::Relaxed);
    s.written.store(7, Ordering::Relaxed);
    s.dropped.store(3, Ordering::Relaxed);
    s.deadbanded.store(5, Ordering::Relaxed);
    s.deadband_evaluated.store(8, Ordering::Relaxed);
    s.skipped.store(2, Ordering::Relaxed);
    s.lane_mismatch.store(1, Ordering::Relaxed);
    s.errors.store(9, Ordering::Relaxed);
    s.error_rows[0].store(3, Ordering::Relaxed); // memory_budget
    s.error_rows[2].store(1, Ordering::Relaxed); // wal_disk
    s.error_rows[4].store(5, Ordering::Relaxed); // other
    Arc::new(s)
}

/// 固定读数的假 telemetry（used=111 / budget=256MiB / series=7——互异且
/// budget 字面即 268435456，断言防串位；Task 6 扩：内存五分量与 admission
/// 缺口固定读数，同样互异防串位）。
struct FakeTelemetry;

impl rs_iot::history::HistoryTelemetry for FakeTelemetry {
    fn memory_used_bytes(&self) -> u64 {
        111
    }
    fn memory_budget_bytes(&self) -> u64 {
        268_435_456
    }
    fn series_count(&self) -> u64 {
        7
    }
    fn memory_components(&self) -> rs_iot::history::MemoryComponents {
        rs_iot::history::MemoryComponents {
            registry_bytes: 1_000,
            metadata_cache_bytes: 2_000,
            persisted_index_bytes: 3_000,
            persisted_mmap_bytes: 4_000,
            active_and_sealed_bytes: 5_000,
        }
    }
    fn admission_memory_gap(&self) -> rs_iot::history::AdmissionMemoryGap {
        rs_iot::history::AdmissionMemoryGap {
            rejections_total: 2,
            last_estimate_bytes: 6_000,
            last_shortfall_bytes: 7_000,
        }
    }
}

/// 构造注入指定 history 测试态的 axum app（其余照 metrics_e2e 范本）。
///
/// data_dir 每次调用唯一（pid+纳秒，dataflow `unique_dir` 先例）：lux 启动
/// 时从 data_dir 加载持久化快照，治理用例注册模型后若复用固定目录，跨测试/
/// 跨运行的模型残留会污染 archived_tags 计数（model_registry_e2e 同理由先
/// 整删目录，此处直接唯一化更简）。附带返回 lux client 供用例注册模型。
async fn test_app(
    history_stats: Option<Arc<HistoryStats>>,
    history_engine_state: Arc<str>,
    history_telemetry: Option<Arc<dyn rs_iot::history::HistoryTelemetry>>,
) -> (axum::Router, lux::ServerHandle, lux::EmbeddedClient) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_nanos();
    let data_dir = common::test_data_dir(&format!("hist_vis-{}-{nanos}", std::process::id()));
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
        history_stats,
        history_engine_state,
        history_reader: None,
        history_telemetry,
        rtdata_watch: std::sync::Arc::new(rs_iot::api::rtdata_watch::RtdataWatchService::new()),
    };
    (build_router(state), handle, client)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("json body")
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

/// 登录拿 JWT（sysstatus 受保护，照 api_flow 范本）。
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

/// 注册恰好 1 个 history=true tag 的模型（经 migrate convert 全链路，dataflow
/// e2e `register_history_model` 同款惯例），供 `archived_tags` 计数。两治理
/// 用例用互异 devid/inst 命名空间（effective 缓存为进程级全局，快照最后
/// reload 者胜——两用例各恰 1 个 history=true tag，任一快照胜出计数均为 1）。
/// 注册后调用方须 cache::reload 让 effective 生效（返回值无法表达该义务，去
/// must_use 以免 -D unused-must_use 误伤 () 返回）。
async fn register_one_archived_tag_model(client: &lux::EmbeddedClient, devid: &str, inst_id: &str) {
    let _bindings = common::register_devices_as_models(
        client,
        inst_id,
        "dataSim",
        vec![DeviceSpec {
            label: "治理可见性e2e设备".into(),
            value: devid.into(),
            children: vec![ChildSpec {
                label: "归档tag".into(),
                value: "tagA".into(),
                r#type: "float".into(),
                props: Some(TagProps {
                    history: true,
                    ..TagProps::default()
                }),
            }],
            slave_id: None,
        }],
    )
    .await;
}

/// /sysstatus：Some(stats) + engine="ok" → history 块 feature/enabled/engine
/// 与七计数器值精确断言（配置默认 enabled=false 与句柄在并存——注入态语义：
/// 配置关但句柄在时 stats 仍真实）。
#[tokio::test]
async fn sysstatus_history_block_ok() {
    let (app, handle, _client) = test_app(Some(preset_stats()), Arc::from("ok"), None).await;
    let token = login_token(&app).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/sysstatus")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let hist = &json["history"];
    assert_eq!(hist["feature"], true, "测试带 history feature 编译,恒 true");
    assert_eq!(hist["enabled"], false, "默认配置 enabled=false");
    assert_eq!(hist["engine"], "ok");
    let stats = &hist["stats"];
    assert!(
        stats.is_object(),
        "engine==ok 时 stats 非 null,实际: {stats}"
    );
    assert_eq!(stats["sent"], 11);
    assert_eq!(stats["written"], 7);
    assert_eq!(stats["dropped"], 3);
    assert_eq!(stats["deadbanded"], 5);
    assert_eq!(stats["skipped"], 2);
    assert_eq!(stats["lane_mismatch"], 1);
    assert_eq!(stats["errors"], 9);

    common::stop_lux(handle).await;
}

/// /sysstatus：stats=None + engine="disabled" → feature 仍 true（测试带 feature
/// 编译）、stats 恒 null（仅 engine=="ok" 时非 null）。
#[tokio::test]
async fn sysstatus_history_block_absent() {
    let (app, handle, _client) = test_app(None, Arc::from("disabled"), None).await;
    let token = login_token(&app).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/sysstatus")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let hist = &json["history"];
    assert_eq!(hist["feature"], true);
    assert_eq!(hist["engine"], "disabled");
    assert!(
        hist["stats"].is_null(),
        "engine 非 ok 时 stats 恒 null,实际: {}",
        hist["stats"]
    );

    common::stop_lux(handle).await;
}

/// /metrics：Some(stats) → 7 个 `rs_iot_history_*_total` family 头 + 正确数值；
/// None 态 → 全部 7 个不出现（引擎非 ok 不输出，避免误导零值）。
#[tokio::test]
async fn metrics_history_families() {
    let families: [(&str, &str); 7] = [
        ("rs_iot_history_sent_total", "11"),
        ("rs_iot_history_written_total", "7"),
        ("rs_iot_history_dropped_total", "3"),
        ("rs_iot_history_deadbanded_total", "5"),
        ("rs_iot_history_skipped_total", "2"),
        ("rs_iot_history_lane_mismatch_total", "1"),
        ("rs_iot_history_errors_total", "9"),
    ];

    // Some(stats) 态：7 family 头 + 数值行齐全
    let (app, handle, _client) = test_app(Some(preset_stats()), Arc::from("ok"), None).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK, "/metrics 免 JWT");
    let body = body_text(resp).await;
    for (name, val) in families {
        assert!(
            body.contains(&format!("# TYPE {name} counter")),
            "{name} family 头缺失:\n{body}"
        );
        assert!(
            body.contains(&format!("{name} {val}")),
            "{name} 数值行缺失(期望 {val}):\n{body}"
        );
    }
    common::stop_lux(handle).await;

    // None 态：7 family 全部不出现
    let (app, handle, _client) = test_app(None, Arc::from("disabled"), None).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    for (name, _) in families {
        assert!(
            !body.contains(name),
            "{name} 不应出现(引擎非 ok,防误导零值):\n{body}"
        );
    }
    common::stop_lux(handle).await;
}

/// /metrics：引擎 ok + telemetry 注入 + 恰 1 个 history=true tag → 治理
/// 七 family 全量输出（Phase 3 P1/P2/P3）：
/// - 四水位 gauge（archived_tags/series/memory_used_bytes/memory_budget_bytes）
///   值取 FakeTelemetry 固定读数与 effective 计数；
/// - 死区评估 counter 与削减率 gauge（P2：ratio = deadbanded/evaluated =
///   5/8 = 0.625——恰为 f64 精确二进制表示，Rust 最短回转 Display 与
///   {:.3} 格式化均为 "0.625"，可精确断言整行）；
/// - 写错误五桶 labeled counter（P3：Σkind == errors == 9，0 值桶也输出）。
#[tokio::test]
async fn metrics_history_governance_families() {
    let (app, handle, client) = test_app(
        Some(preset_stats()),
        Arc::from("ok"),
        Some(Arc::new(FakeTelemetry)),
    )
    .await;

    // 注册恰 1 个 history=true tag 模型并物化 effective（archived_tags 数据源）
    register_one_archived_tag_model(&client, "hvis_g1", "histvis_g").await;
    let effective = rs_iot::model::cache::reload(&client)
        .await
        .expect("reload effective 必须成功");
    assert_eq!(
        effective.get("hvis_g1.tagA").map(|p| p.history),
        Some(true),
        "构造校验：hvis_g1.tagA 必须在 effective 且 history=true"
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK, "/metrics 免 JWT");
    let body = body_text(resp).await;

    // 十四个无 label family：HELP/TYPE 头 + 精确数值行
    let gov_families: [(&str, &str, &str); 14] = [
        (
            "rs_iot_history_archived_tags",
            "gauge",
            "rs_iot_history_archived_tags 1",
        ),
        ("rs_iot_history_series", "gauge", "rs_iot_history_series 7"),
        (
            "rs_iot_history_memory_used_bytes",
            "gauge",
            "rs_iot_history_memory_used_bytes 111",
        ),
        (
            "rs_iot_history_memory_budget_bytes",
            "gauge",
            "rs_iot_history_memory_budget_bytes 268435456",
        ),
        (
            "rs_iot_history_deadband_evaluated_total",
            "counter",
            "rs_iot_history_deadband_evaluated_total 8",
        ),
        (
            "rs_iot_history_deadband_reduction_ratio",
            "gauge",
            "rs_iot_history_deadband_reduction_ratio 0.625",
        ),
        // Task 6 内存分量 gauge（FakeTelemetry 固定读数，互异千位值防串位）
        (
            "rs_iot_history_memory_registry_bytes",
            "gauge",
            "rs_iot_history_memory_registry_bytes 1000",
        ),
        (
            "rs_iot_history_memory_metadata_bytes",
            "gauge",
            "rs_iot_history_memory_metadata_bytes 2000",
        ),
        (
            "rs_iot_history_memory_persisted_index_bytes",
            "gauge",
            "rs_iot_history_memory_persisted_index_bytes 3000",
        ),
        (
            "rs_iot_history_memory_persisted_mmap_bytes",
            "gauge",
            "rs_iot_history_memory_persisted_mmap_bytes 4000",
        ),
        (
            "rs_iot_history_memory_active_sealed_bytes",
            "gauge",
            "rs_iot_history_memory_active_sealed_bytes 5000",
        ),
        // Task 6 admission 缺口（拒绝 counter + 预估/缺口 gauge）
        (
            "rs_iot_history_admission_rejections_total",
            "counter",
            "rs_iot_history_admission_rejections_total 2",
        ),
        (
            "rs_iot_history_admission_last_estimate_bytes",
            "gauge",
            "rs_iot_history_admission_last_estimate_bytes 6000",
        ),
        (
            "rs_iot_history_admission_shortfall_bytes",
            "gauge",
            "rs_iot_history_admission_shortfall_bytes 7000",
        ),
    ];
    for (name, ty, line) in gov_families {
        assert!(
            body.contains(&format!("# HELP {name} ")),
            "{name} HELP 行缺失:\n{body}"
        );
        assert!(
            body.contains(&format!("# TYPE {name} {ty}")),
            "{name} TYPE 行缺失(期望 {ty}):\n{body}"
        );
        assert!(body.contains(line), "数值行缺失(期望 \"{line}\"):\n{body}");
    }

    // 写错误五桶：family 头一对 + 逐 kind 精确行（值 0 桶也须输出，P3 齐五桶）
    assert!(
        body.contains("# HELP rs_iot_history_write_errors_total "),
        "write_errors_total HELP 行缺失:\n{body}"
    );
    assert!(
        body.contains("# TYPE rs_iot_history_write_errors_total counter"),
        "write_errors_total TYPE 行缺失:\n{body}"
    );
    let err_kinds: [(&str, &str); 5] = [
        ("memory_budget", "3"),
        ("cardinality", "0"),
        ("wal_disk", "1"),
        ("closed", "0"),
        ("other", "5"),
    ];
    for (kind, val) in err_kinds {
        assert!(
            body.contains(&format!(
                "rs_iot_history_write_errors_total{{kind=\"{kind}\"}} {val}"
            )),
            "write_errors_total {kind} 桶数值行缺失(期望 {val}):\n{body}"
        );
    }

    common::stop_lux(handle).await;
}

/// /metrics：引擎非 ok（disabled/stats=None/telemetry=None）→ 治理七 family
/// 全部不出现（仅引擎 ok 契约，与既有 7 family 同语义——零值会误导抓取方）。
#[tokio::test]
async fn metrics_history_governance_omitted_when_not_ok() {
    let (app, handle, _client) = test_app(None, Arc::from("disabled"), None).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;

    let new_families = [
        "rs_iot_history_archived_tags",
        "rs_iot_history_series",
        "rs_iot_history_memory_used_bytes",
        "rs_iot_history_memory_budget_bytes",
        "rs_iot_history_deadband_evaluated_total",
        "rs_iot_history_deadband_reduction_ratio",
        "rs_iot_history_write_errors_total",
        // Task 6：内存分量与 admission 缺口 family 同缺席语义
        "rs_iot_history_memory_registry_bytes",
        "rs_iot_history_memory_metadata_bytes",
        "rs_iot_history_memory_persisted_index_bytes",
        "rs_iot_history_memory_persisted_mmap_bytes",
        "rs_iot_history_memory_active_sealed_bytes",
        "rs_iot_history_admission_rejections_total",
        "rs_iot_history_admission_last_estimate_bytes",
        "rs_iot_history_admission_shortfall_bytes",
    ];
    for name in new_families {
        assert!(
            !body.contains(name),
            "{name} 不应出现(引擎非 ok,治理 family 同省略契约):\n{body}"
        );
    }
    // 既有 7 family 也不出现（现状契约，与 metrics_history_families None 态一致）
    let legacy_families = [
        "rs_iot_history_sent_total",
        "rs_iot_history_written_total",
        "rs_iot_history_dropped_total",
        "rs_iot_history_deadbanded_total",
        "rs_iot_history_skipped_total",
        "rs_iot_history_lane_mismatch_total",
        "rs_iot_history_errors_total",
    ];
    for name in legacy_families {
        assert!(
            !body.contains(name),
            "{name} 不应出现(引擎非 ok,防误导零值):\n{body}"
        );
    }
    common::stop_lux(handle).await;
}

/// /sysstatus：同治理 ok 态（preset + FakeTelemetry + 恰 1 个 history=true
/// tag）→ history 块 P6 新字段精确断言：水位/series/archived_tags/deadbands
/// 与 stats 的 deadband_evaluated/errors_by_kind（字段命名按计划 P6）。
#[tokio::test]
async fn sysstatus_history_governance_fields() {
    let (app, handle, client) = test_app(
        Some(preset_stats()),
        Arc::from("ok"),
        Some(Arc::new(FakeTelemetry)),
    )
    .await;

    register_one_archived_tag_model(&client, "hvis_s1", "histvis_s").await;
    let effective = rs_iot::model::cache::reload(&client)
        .await
        .expect("reload effective 必须成功");
    assert_eq!(
        effective.get("hvis_s1.tagA").map(|p| p.history),
        Some(true),
        "构造校验：hvis_s1.tagA 必须在 effective 且 history=true"
    );

    let token = login_token(&app).await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/sysstatus")
                .header(header::AUTHORIZATION, &token)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("call app");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let hist = &json["history"];
    assert_eq!(hist["engine"], "ok");

    // P6 治理字段：水位/series（telemetry 口径）+ archived_tags（effective 计数）
    assert_eq!(hist["memory_used_bytes"], 111);
    assert_eq!(hist["memory_budget_bytes"], 268_435_456u64);
    assert_eq!(hist["series"], 7);
    assert_eq!(hist["archived_tags"], 1);
    // 死区削减率（P2 同 /metrics 口径：5/8 = 0.625）
    assert_eq!(hist["deadbands"]["evaluated"], 8);
    assert_eq!(hist["deadbands"]["ratio"], 0.625);

    // stats 扩展：评估计数 + 五桶分型（整对象等值锁键集与值，Σ=9==errors）
    let stats = &hist["stats"];
    assert_eq!(stats["deadband_evaluated"], 8);
    assert_eq!(
        stats["errors_by_kind"],
        serde_json::json!({
            "memory_budget": 3,
            "cardinality": 0,
            "wal_disk": 1,
            "closed": 0,
            "other": 5,
        }),
        "五桶键集与值精确(整对象等值)"
    );

    common::stop_lux(handle).await;
}

/// /sysstatus 运行态上行（P4 僵尸告警）：history 块新增 `degraded` 字段反映
/// writer 错误路径锁存的 `closed_latched`（Closed 桶首增——引擎僵尸/关停）。
/// 同一 stats 对象先读（未锁存 degraded=false）、置位后再读（degraded=true），
/// 锁死「静态三态 `engine` 与运行态 `degraded` 分离」：旧字段语义不动（前端
/// 兼容，engine 仍报 ok），新增字段纯增量。
#[tokio::test]
async fn sysstatus_history_degraded_reflects_closed_latch() {
    let stats = preset_stats();
    let (app, handle, _client) = test_app(Some(stats.clone()), Arc::from("ok"), None).await;
    let token = login_token(&app).await;

    let get_hist = |app: axum::Router, token: String| async move {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/sysstatus")
                    .header(header::AUTHORIZATION, &token)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("call app");
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        json["history"].clone()
    };

    // 未锁存：degraded=false，engine 旧三态字段不动
    let hist = get_hist(app.clone(), token.clone()).await;
    assert_eq!(hist["degraded"], false, "未锁存 degraded 必须 false");
    assert_eq!(hist["engine"], "ok", "旧字段语义不动");

    // writer 错误路径置位锁存（模拟 Closed 桶首增）→ degraded=true 上行
    stats.closed_latched.store(true, Ordering::SeqCst);
    let hist = get_hist(app, token).await;
    assert_eq!(
        hist["degraded"], true,
        "closed_latched 置位必须上行 degraded=true（引擎僵尸不再 sysstatus 报 ok 无异常）"
    );
    assert_eq!(hist["engine"], "ok", "engine 静态三态与运行态分离，不联动");

    common::stop_lux(handle).await;
}
