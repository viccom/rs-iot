//! 实时数据订阅端到端：OPC UA 式语义（snapshot 全量 → 周期 datachange 只推变化）。
//!
//! 覆盖：JWT 保护、snapshot 全量+过滤、值变才推（值同不推）、设备删除 removed。
//! 测试基建对标 tests/events_e2e.rs（同 test_app 骨架）；SSE 读流用窗口收集断言
//! （先收 snapshot 窗口，再变更数据，再收 datachange 窗口）。
//!
//! Task 5（P11 双 impl 等价）：主干用例对 ksub / poll 两档各跑一遍——ksub 档 =
//! config `[rtdata] sub_impl=ksub` 且测试侧 spawn watcher（复刻 lib.rs 编排，P8）；
//! poll 档 = `sub_impl=poll`、服务构造但不 spawn。两档断言逐字一致，防单边漂移
//! （R3）。另含 ksub 专项（新设备事件发现 / DEL removed / 多连接隔离）与 poll
//! 守门。

mod common;

use axum::body::{Body, BodyDataStream};
use axum::http::{header, Request, StatusCode};
use futures_util::StreamExt;
use rs_iot::api::{build_router, AppState};
use rs_iot::event_bus::EventBus;
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::log_ringbuffer::LogRingBuffer;
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{Hwid, Sample, SampleValue};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// 建测试 app（订阅端点读 lux，不依赖 EventBus 事件流；骨架同 events_e2e 便于对照）。
///
/// 仅供与 impl 档无关的骨架用例（401 / 200+content-type：鉴权与握手先于任何
/// 读源分支，P11 参数化不增加覆盖只翻倍运行时，故不参数化）。
async fn test_app(name: &str) -> (axum::Router, lux::ServerHandle, lux::EmbeddedClient) {
    let data_dir = common::test_data_dir(name);
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
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
        metrics: metrics.clone(),
        trace_registry,
        log_ringbuffer: Arc::new(LogRingBuffer::new(2000)),
        event_bus,
        config: std::sync::Arc::new(rs_iot::config::AppConfig::default()),
        history_stats: None,
        history_engine_state: std::sync::Arc::from("disabled"),
        history_reader: None,
        history_telemetry: None,
        rtdata_watch: std::sync::Arc::new(rs_iot::api::rtdata_watch::RtdataWatchService::new()),
    };
    (build_router(state), handle, client)
}

// ---------- P11 双 impl 档 ----------

/// 主干用例的双 impl 档（P11）：同一断言 ksub / poll 各跑一遍。
#[derive(Clone, Copy)]
enum Tier {
    /// config `sub_impl=ksub` 且测试侧 spawn watcher（复刻 lib.rs 编排，P8）。
    Ksub,
    /// config `sub_impl=poll`，watch 服务构造但不 spawn（lux 无订阅者零开销承诺）。
    Poll,
}

impl Tier {
    /// data_dir 后缀——两档独立 lux 数据目录互不污染。
    fn label(self) -> &'static str {
        match self {
            Tier::Ksub => "ksub",
            Tier::Poll => "poll",
        }
    }

    fn sub_impl(self) -> rs_iot::config::SubImpl {
        match self {
            Tier::Ksub => rs_iot::config::SubImpl::Ksub,
            Tier::Poll => rs_iot::config::SubImpl::Poll,
        }
    }
}

/// 独立临时目录并清空上一轮残留——lux data_dir 跨 run 持久化（lux.dat + WAL），
/// 不清会让存量键漏进本轮断言造成假绿/假红（rtdata_watch_e2e fresh_dir 先例）。
fn fresh_dir(name: &str) -> std::path::PathBuf {
    let d = common::test_data_dir(name);
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// 双档测试 app：config 注入 `[rtdata] sub_impl`；ksub 档测试侧 spawn watcher
/// （等价 lib.rs `sub_impl==ksub` 才 spawn 的编排分支，P8），poll 档构造服务
/// 但不 spawn。返回 (router, lux handle, lux client, watch 服务, watcher 取消 token)。
async fn test_app_tier(
    name: &str,
    tier: Tier,
) -> (
    axum::Router,
    lux::ServerHandle,
    lux::EmbeddedClient,
    Arc<rs_iot::api::rtdata_watch::RtdataWatchService>,
    CancellationToken,
) {
    let data_dir = fresh_dir(&format!("{name}-{}", tier.label()));
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
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
        history: None,
    };
    let (hub_tx, _hub_rx) = tokio::sync::mpsc::channel(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));
    let rtdata_watch = Arc::new(rs_iot::api::rtdata_watch::RtdataWatchService::new());
    let watch_token = CancellationToken::new();
    if matches!(tier, Tier::Ksub) {
        // 命名绑定 `_watcher` 满足 JoinHandle must_use（lib.rs 同惯例）。
        let _watcher = rtdata_watch.spawn(client.clone(), watch_token.clone());
    }
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
        config: Arc::new(rs_iot::config::AppConfig {
            rtdata: rs_iot::config::RtdataConfig {
                sub_impl: tier.sub_impl(),
            },
            ..Default::default()
        }),
        history_stats: None,
        history_engine_state: Arc::from("disabled"),
        history_reader: None,
        history_telemetry: None,
        rtdata_watch: rtdata_watch.clone(),
    };
    (
        build_router(state),
        handle,
        client,
        rtdata_watch,
        watch_token,
    )
}

/// 登录拿 JWT（admin/admin）。
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

/// 建订阅连接（带 JWT）：断言 200 + text/event-stream，返回 body 流。
async fn connect(app: &axum::Router, query: &str, tok: &str) -> BodyDataStream {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/rtdatas/subscribe{query}"))
                .header(header::AUTHORIZATION, tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    resp.into_body().into_data_stream()
}

/// 收集窗口：读流 dur 时长（或流尽）→ String（窗口式断言基建，events_e2e 内联模式的函数化）。
async fn collect_window(stream: &mut BodyDataStream, dur: std::time::Duration) -> String {
    let mut buf = String::new();
    let _ = tokio::time::timeout(dur, async {
        while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
    })
    .await;
    buf
}

/// 种子数据：write_batch 一条 Sample（field 形如 <devid>.<tag>，cell[2] 为值串）。
async fn seed(lux: &lux::EmbeddedClient, devid: &str, pts: &[(&str, &str)], ts: i64) {
    let mut values = BTreeMap::new();
    for (t, v) in pts {
        values.insert((*t).to_string(), SampleValue::Str((*v).to_string()));
    }
    let s = Sample {
        hwid: Hwid::new("it-hwid"),
        devid: devid.into(),
        values,
        timestamp: ts,
    };
    rs_iot::db::rtdata::write_batch(lux, std::slice::from_ref(&s))
        .await
        .unwrap();
}

// 与 impl 档无关（鉴权拒绝先于 handler 主体，任何读源下同语义），不参数化。
#[tokio::test]
async fn subscribe_without_jwt_401() {
    let (app, handle, _lux) = test_app("sub401").await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/rtdatas/subscribe")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    common::stop_lux(handle).await;
}

// 与 impl 档无关（200 + content-type 握手骨架，空流不卡死即可），不参数化。
#[tokio::test]
async fn subscribe_with_jwt_200_event_stream() {
    let (app, handle, _lux) = test_app("sub200").await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "", &tok).await;
    // 骨架（空流）下流立即结束——窗口收集返回空或 keep-alive，不卡死即可
    // （原 <1MB 断言恒真无守门价值，审查 Phase2-L1）；强化：空库连接不得
    // 推任何数据帧（snapshot/datachange 出现即骨架破例）。
    let buf = collect_window(&mut stream, std::time::Duration::from_millis(300)).await;
    assert!(
        !buf.contains("event: datachange") && !buf.contains("event: snapshot"),
        "空库连接不得推数据帧: {buf}"
    );
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn snapshot_pushes_all_points_on_connect() {
    for tier in [Tier::Ksub, Tier::Poll] {
        snapshot_pushes_all_points_on_connect_case(tier).await;
    }
}

/// 主体（P11 双档：两档独立 lux/服务/AppState/连接，断言逐字一致）。
async fn snapshot_pushes_all_points_on_connect_case(tier: Tier) {
    let (app, handle, lux, _watch, token) = test_app_tier("subsnap", tier).await;
    seed(&lux, "devA", &[("t1", "1.5"), ("t2", "2")], 1_700_000_000).await;
    seed(&lux, "devB", &[("t1", "3")], 1_700_000_000).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?interval=200", &tok).await;
    let buf = collect_window(&mut stream, std::time::Duration::from_millis(800)).await;
    // 两设备各一帧 snapshot；cell 形如 [ts,"localTime","value"]（值串，与 /rtdatas 同构）。
    assert!(buf.contains("event: snapshot"), "应含 snapshot 帧: {buf}");
    assert!(
        buf.contains("\"dev\":\"devA\"") && buf.contains("\"dev\":\"devB\""),
        "逐设备分帧: {buf}"
    );
    assert!(buf.contains("devA.t1") && buf.contains("1.5"), "{buf}");
    assert!(buf.contains("devA.t2") && buf.contains("devB.t1"), "{buf}");
    token.cancel();
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn snapshot_respects_dev_and_tag_filter() {
    for tier in [Tier::Ksub, Tier::Poll] {
        snapshot_respects_dev_and_tag_filter_case(tier).await;
    }
}

/// 主体（P11 双档：两档独立 lux/服务/AppState/连接，断言逐字一致）。
async fn snapshot_respects_dev_and_tag_filter_case(tier: Tier) {
    let (app, handle, lux, _watch, token) = test_app_tier("subfilter", tier).await;
    seed(&lux, "devA", &[("temp", "1"), ("hum", "2")], 1).await;
    seed(&lux, "devB", &[("temp", "3")], 1).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?dev_id=devA&tag=temp&interval=200", &tok).await;
    let buf = collect_window(&mut stream, std::time::Duration::from_millis(800)).await;
    assert!(buf.contains("devA.temp"), "{buf}");
    assert!(!buf.contains("devA.hum"), "tag 过滤应排除: {buf}");
    assert!(!buf.contains("devB"), "dev 过滤应排除 devB: {buf}");
    token.cancel();
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn datachange_only_pushes_changed_values() {
    for tier in [Tier::Ksub, Tier::Poll] {
        datachange_only_pushes_changed_values_case(tier).await;
    }
}

/// 主体（P11 双档：两档独立 lux/服务/AppState/连接，断言逐字一致）。
async fn datachange_only_pushes_changed_values_case(tier: Tier) {
    let (app, handle, lux, _watch, token) = test_app_tier("subdiff", tier).await;
    seed(&lux, "devA", &[("t1", "1"), ("t2", "2")], 1_700_000_000).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?dev_id=devA&interval=100", &tok).await;
    // 窗口1：snapshot 收全。
    let buf1 = collect_window(&mut stream, std::time::Duration::from_millis(600)).await;
    assert!(
        buf1.contains("event: snapshot") && buf1.contains("devA.t1"),
        "{buf1}"
    );
    // 变更：t1 新值；t2 同值不同 ts（采集周期写回同值场景）。
    seed(&lux, "devA", &[("t1", "9"), ("t2", "2")], 1_700_000_100).await;
    let buf2 = collect_window(&mut stream, std::time::Duration::from_millis(1500)).await;
    assert!(
        buf2.contains("event: datachange"),
        "应含 datachange: {buf2}"
    );
    assert!(buf2.contains("devA.t1") && buf2.contains("9"), "{buf2}");
    assert!(!buf2.contains("devA.t2"), "值未变不应重推: {buf2}");
    token.cancel();
    common::stop_lux(handle).await;
}

#[tokio::test]
async fn removed_on_device_delete() {
    for tier in [Tier::Ksub, Tier::Poll] {
        removed_on_device_delete_case(tier).await;
    }
}

/// 主体（P11 双档：两档独立 lux/服务/AppState/连接，断言逐字一致）。
async fn removed_on_device_delete_case(tier: Tier) {
    let (app, handle, lux, _watch, token) = test_app_tier("subdel", tier).await;
    seed(&lux, "devA", &[("t1", "1")], 1).await;
    seed(&lux, "devB", &[("t1", "2")], 1).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?interval=100", &tok).await;
    let buf1 = collect_window(&mut stream, std::time::Duration::from_millis(600)).await;
    assert!(
        buf1.contains("devA.t1") && buf1.contains("devB.t1"),
        "{buf1}"
    );
    rs_iot::db::rtdata::delete_device(&lux, "devA")
        .await
        .unwrap();
    let buf2 = collect_window(&mut stream, std::time::Duration::from_millis(1500)).await;
    assert!(
        buf2.contains("event: datachange") && buf2.contains("removed"),
        "{buf2}"
    );
    assert!(buf2.contains("devA.t1"), "removed 应列 devA.t1: {buf2}");
    // M5：JSON 解析断言（替代 contains 串的模糊面）——removed 必含 devA.t1
    // 且不得误报非脏设备 devB.t1（project_baseline 投影的 e2e 守门：删除
    // 投影直接喂全量基线时，ksub 档此断言红——poll 档不受影响，其 current
    // 本就全量）。
    let data_line = buf2
        .lines()
        .find(|l| l.starts_with("data:") && l.contains("\"removed\""))
        .expect("必有一帧含 removed");
    let payload: serde_json::Value =
        serde_json::from_str(data_line.trim_start_matches("data:").trim())
            .expect("datachange 帧必须是合法 JSON");
    let removed = payload["removed"].as_array().expect("removed 须为数组");
    assert!(
        removed.iter().any(|v| v == "devA.t1"),
        "removed 必含 devA.t1: {payload}"
    );
    assert!(
        !removed.iter().any(|v| v == "devB.t1"),
        "非脏设备 devB 不得整域误报 removed（project_baseline 守门）: {payload}"
    );
    token.cancel();
    common::stop_lux(handle).await;
}

// ---------- Task 5 新增专项（ksub 档为主） ----------

/// ksub 专项：dev 过滤=None 的连接建立后新设备写入 → 靠 key 事件进版本表被发现
/// （等价原 KEYS 发现，P9）——newdev 不依赖启动铺底（连接之后才出现，纯事件发现）。
#[tokio::test]
async fn ksub_new_device_discovered_without_keys() {
    let (app, handle, lux, watch, token) = test_app_tier("ksub-newdev", Tier::Ksub).await;
    seed(&lux, "devA", &[("t1", "1")], 1).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?interval=200", &tok).await;
    let buf1 = collect_window(&mut stream, std::time::Duration::from_millis(600)).await;
    assert!(
        buf1.contains("event: snapshot") && buf1.contains("devA.t1"),
        "{buf1}"
    );
    assert!(!buf1.contains("newdev"), "连接时 newdev 尚不存在: {buf1}");
    // 连接后新设备首写：watcher 收 HSET key 事件入版本表（ksub 档契约前提，
    // 保证本档 watcher 确已 spawn 且活着——否则下方 SSE 断言无从谈起）。
    seed(&lux, "newdev", &[("n1", "7")], 2).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while watch.version("newdev").is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("ksub 档契约：watcher 应已收到 newdev 的 key 事件");
    let buf2 = collect_window(&mut stream, std::time::Duration::from_millis(1500)).await;
    assert!(
        buf2.contains("event: datachange"),
        "新设备应进 datachange: {buf2}"
    );
    assert!(
        buf2.contains("newdev.n1") && buf2.contains("7"),
        "等价原 KEYS 发现——新设备点位应推送: {buf2}"
    );
    token.cancel();
    common::stop_lux(handle).await;
}

/// ksub 专项：DEL rtdata:devA 也是 key 事件 → mark → HGETALL 空 → diff_scope 出
/// removed（P9，等价原轮询语义；版本表不删项）。用原始 DEL 命令锚定 DEL 事件路径
/// （watch_service_marks_del 的订阅面下游）。
#[tokio::test]
async fn ksub_removed_via_del_event() {
    let (app, handle, lux, _watch, token) = test_app_tier("ksub-del", Tier::Ksub).await;
    seed(&lux, "devA", &[("t1", "1")], 1).await;
    seed(&lux, "devB", &[("t1", "2")], 1).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?interval=200", &tok).await;
    let buf1 = collect_window(&mut stream, std::time::Duration::from_millis(600)).await;
    assert!(
        buf1.contains("devA.t1") && buf1.contains("devB.t1"),
        "{buf1}"
    );
    lux.execute("DEL", &["rtdata:devA"]).await.unwrap();
    let buf2 = collect_window(&mut stream, std::time::Duration::from_millis(1500)).await;
    assert!(
        buf2.contains("event: datachange") && buf2.contains("removed"),
        "{buf2}"
    );
    assert!(buf2.contains("devA.t1"), "removed 应列 devA.t1: {buf2}");
    token.cancel();
    common::stop_lux(handle).await;
}

/// ksub 专项：两连接不同 dev 过滤 + interval → 各自独立基线/游标（per-connection
/// last_versions/last_epoch，P7），只收各自范围；一连接断开另一连接继续收帧。
#[tokio::test]
async fn ksub_multi_connection_isolation() {
    let (app, handle, lux, _watch, token) = test_app_tier("ksub-multi", Tier::Ksub).await;
    seed(&lux, "devA", &[("t1", "1")], 1).await;
    seed(&lux, "devB", &[("t1", "1")], 1).await;
    let tok = jwt(&app).await;
    let mut s1 = connect(&app, "?dev_id=devA&interval=200", &tok).await;
    let mut s2 = connect(&app, "?dev_id=devB&interval=300", &tok).await;
    let b1 = collect_window(&mut s1, std::time::Duration::from_millis(600)).await;
    let b2 = collect_window(&mut s2, std::time::Duration::from_millis(600)).await;
    assert!(
        b1.contains("devA.t1") && !b1.contains("devB"),
        "conn1 只含 devA: {b1}"
    );
    assert!(
        b2.contains("devB.t1") && !b2.contains("devA"),
        "conn2 只含 devB: {b2}"
    );
    // devA 值变：conn1 收 datachange，conn2（只订 devB）不收。
    seed(&lux, "devA", &[("t1", "9")], 2).await;
    let w1 = collect_window(&mut s1, std::time::Duration::from_millis(1500)).await;
    let w2 = collect_window(&mut s2, std::time::Duration::from_millis(1500)).await;
    assert!(
        w1.contains("event: datachange") && w1.contains("devA.t1") && w1.contains("9"),
        "{w1}"
    );
    assert!(!w2.contains("devA"), "conn2 不订 devA，不应收到: {w2}");
    // conn1 断开（drop 流 → producer 退出）；conn2 继续独立收帧。
    drop(s1);
    seed(&lux, "devB", &[("t1", "8")], 3).await;
    let w3 = collect_window(&mut s2, std::time::Duration::from_millis(1500)).await;
    assert!(
        w3.contains("event: datachange") && w3.contains("devB.t1") && w3.contains("8"),
        "conn1 断开后 conn2 仍应收帧: {w3}"
    );
    token.cancel();
    common::stop_lux(handle).await;
}

/// poll 守门（P8 回退开关）：显式 `sub_impl=poll` 走原 read_scope 轮询路径——
/// snapshot + datachange 主干必须始终可用（本测试现在就应绿）。
#[tokio::test]
async fn poll_impl_still_works() {
    let (app, handle, lux, _watch, token) = test_app_tier("poll-guard", Tier::Poll).await;
    seed(&lux, "devA", &[("t1", "1")], 1_700_000_000).await;
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?dev_id=devA&interval=100", &tok).await;
    let buf1 = collect_window(&mut stream, std::time::Duration::from_millis(600)).await;
    assert!(
        buf1.contains("event: snapshot") && buf1.contains("devA.t1"),
        "{buf1}"
    );
    seed(&lux, "devA", &[("t1", "9")], 1_700_000_100).await;
    let buf2 = collect_window(&mut stream, std::time::Duration::from_millis(1500)).await;
    assert!(
        buf2.contains("event: datachange") && buf2.contains("devA.t1") && buf2.contains("9"),
        "{buf2}"
    );
    token.cancel();
    common::stop_lux(handle).await;
}

/// ksub 专项：snapshot 阶段期间发生的值变更不得丢帧——游标基线（last_versions）
/// 必须在 snapshot **之前**建立（`snapshot_versions` 文档语义："先取快照再全量读，
/// 之后的 mark 版本 > 游标，下次 tick 报脏，不丢帧"）。若游标后置到 snapshot 之后，
/// snapshot 半程中已推设备的 mark 会被游标吸收，该变更永不推送（静默旧值）。
///
/// 反压构造（确定性开窗）：20 台设备逐帧推送 > SSE channel 容量 8，producer
/// 必然阻塞在中途 `send`——此刻对全部设备写新值，已推走旧值的 ≥9 台的丢帧窗口
/// 被打开；随后消费使 snapshot 完成。判别：窗口拼接串须含 datachange。
#[tokio::test]
async fn ksub_change_during_snapshot_not_lost() {
    let (app, handle, lux, _watch, token) = test_app_tier("ksub-snapwin", Tier::Ksub).await;
    let devs: Vec<String> = (0..20).map(|i| format!("dev{i:02}")).collect();
    for d in &devs {
        seed(&lux, d, &[("t1", "1")], 1).await;
    }
    let tok = jwt(&app).await;
    let mut stream = connect(&app, "?interval=200", &tok).await;
    // 不消费制造反压：channel 满（容量 8）后 producer 阻塞在中途 send，
    // snapshot 停在半程（≥9 台已推走旧值，其余尚未 HGETALL）。
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // snapshot 半程中对全部设备写新值：已推设备的新值不在其 snapshot 帧。
    for d in &devs {
        seed(&lux, d, &[("t1", "9")], 2).await;
    }
    // 开始消费：排空 channel，snapshot 完成（游标建立时序视实现而定）。
    let buf1 = collect_window(&mut stream, std::time::Duration::from_millis(800)).await;
    assert!(buf1.contains("event: snapshot"), "{buf1}");
    // 判别窗口（首 tick 可能落在本窗或下窗，拼接判）：snapshot 期间已推设备
    // 的值变更必须以 datachange 补推。
    let buf2 = collect_window(&mut stream, std::time::Duration::from_millis(1500)).await;
    let combined = format!("{buf1}{buf2}");
    // 判别强化（审查 Phase2-L2）：原 contains 单帧即绿——补推 1 台也过；改为
    // JSON 解析统计 datachanges 中携带新值 "9" 的不同设备数，构造注释的
    // "≥9 台已推走旧值"下界必须兑现（20 台 − channel 容量 8）。
    let mut updated: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in combined.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        let Some(changes) = v.get("changes").and_then(|c| c.as_object()) else {
            continue;
        };
        for (field, cell) in changes {
            let is_nine = cell
                .as_array()
                .and_then(|a| a.last())
                .and_then(|e| e.as_str())
                .is_some_and(|s| s == "9");
            if is_nine {
                updated.insert(field.clone());
            }
        }
    }
    assert!(
        updated.len() >= 9,
        "snapshot 期间的值变更不得丢帧：新值 9 须补推 ≥9 台（20−8 下界），实际 {} 台: {combined}",
        updated.len()
    );
    token.cancel();
    common::stop_lux(handle).await;
}
