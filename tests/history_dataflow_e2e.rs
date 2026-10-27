//! 历史写路径 e2e（Task 5 红）：pipeline flush_batch 出口旁路 fan_out 全链路
//! （hub → pipeline → 白名单过滤 → writer → tsink 落盘）+ L2 关闭零副作用。
//!
//! 本文件锚定**尚不存在**的公共 API 面（绿阶段按此实现，见
//! docs/designs/2026-08-25-history-write-path-plan.md「Task 5」）：
//! - `HostContext` 新增 pub 字段 `history: Option<history::rows::HistoryHandle>`
//!   （P6：rows 无条件编译，feature off 时字段仍存在、值恒 None 可用）；
//! - `HistoryEngine::handle(&self) -> Option<HistoryHandle>`（writer 存活时
//!   `Some`；`HistoryHandle` 可 `Clone`，clone 语义与 tokio mpsc Sender 一致）；
//! - pipeline `flush_batch` 出口在 `ctx.history == Some` 时调用
//!   `history::fan_out`（经由发送 Sample 后 `stats.written` 增长可观测）。
//!
//! 已存在的基础设施（不重锚定）：`history::engine::maybe_init`/`build_storage`、
//! `engine.close`、`history::rows::fan_out`/`HistoryHandle`/`HistoryStats`、
//! `history::writer::spawn_writer`。
#![cfg(feature = "history")]
// 绿阶段 lint 放行(非断言改动,显式报告)：SERIALIZE 的 std MutexGuard 刻意
// 跨 await 持有——序列化语义要求锁覆盖两用例的整个 async 体(await 点正是
// 被串行的对象)，async Mutex 反而无法在测试间建立"跨完整用例"的排他。
#![allow(clippy::await_holding_lock)]

mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rs_iot::config::HistoryConfig;
use rs_iot::history::engine::{auto_cardinality, build_storage, maybe_init};
use rs_iot::history::rows::HistoryHandle;
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::plugin::traits::Processor;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{ChildSpec, DeviceSpec, Hwid, Sample, SampleValue, TagProps};
use tokio_util::sync::CancellationToken;
use tsink::{Label, SeriesSelection};

/// 同 binary 两用例串行化锁：`model::cache` 的 effective 是进程级全局
/// （OnceLock cell），测试 2 的 store 写路径（register_devices_as_models →
/// set_model/set_device）会以**测试 2 的 lux 连接**触发写路径刷新 → reload
/// 覆盖全局快照；若测试 1 的数据流仍在跑，flush 出口 `fan_out` 读全局
/// effective 会看不到 `hdev1.tagA` 卡（白名单 miss → skip，flaky）。
/// 全局 Mutex 把两用例在本 binary 内串行，消除覆盖竞态（集成测试是独立
/// 进程，跨 binary 无共享，锁只作用于本文件）。
static SERIALIZE: Mutex<()> = Mutex::new(());

/// 取串行锁（poison 容错：前者 panic 不应级联卡死后继用例）。
fn serialize() -> std::sync::MutexGuard<'static, ()> {
    SERIALIZE.lock().unwrap_or_else(|p| p.into_inner())
}

/// 唯一临时目录：tag + pid + 纳秒时钟，测试并行与 CI 并发 run 不冲突。
/// best-effort 清理责任在调用方（Windows 句柄释放有延迟，失败忽略）。
fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rsiot-hist-e2e-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

/// 当前 Unix 秒（Sample ts 基：互异整数秒，防批内去重误伤；30 天默认
/// retention 覆盖 now 附近点，无需放宽）。
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_secs() as i64
}

/// 单样本构造：hwid 与 HostContext 一致（writer series label `hwid` 的值源），
/// values 由 (tag, value) 列表填入。
fn sample(devid: &str, ts: i64, vals: &[(&str, SampleValue)]) -> Sample {
    Sample {
        hwid: Hwid::new("it-hwid"),
        devid: devid.to_string(),
        values: vals
            .iter()
            .map(|(t, v)| ((*t).to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
        timestamp: ts,
    }
}

/// 构造 HostContext（照 tests/dataflow.rs 形态）+ **红点**：`history` 字段
/// 尚不存在于 HostContext——本函数体在绿阶段实现落地前编译不过（E0560）。
fn make_ctx(
    client: &lux::EmbeddedClient,
    shutdown: &CancellationToken,
    history: Option<HistoryHandle>,
) -> HostContext {
    HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(Arc::new(rs_iot::process_metrics::ProcessMetrics::new())),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
        history,
    }
}

/// spawn pipeline（单 worker，deterministic；passthrough 透传）。
/// token 由 pipeline 内部自持（收尾靠 drop hub sender → dispatcher 退出时
/// 级联 cancel workers），测试无需外部 cancel。
fn spawn_pipeline(
    ctx: HostContext,
    hub_rx: tokio::sync::mpsc::Receiver<Sample>,
) -> tokio::task::JoinHandle<()> {
    let processor: Arc<dyn Processor> = Arc::new(Passthrough);
    let token = CancellationToken::new().child_token();
    tokio::spawn(rs_iot::pipeline::run(ctx, hub_rx, processor, 1, token))
}

/// 建一个设备模型：tagA `history=true`、tagB `history=false`。
/// `ChildSpec.props` 经 `model::migrate::convert` 带入 TagTemplate 默认
/// （helper 内部走 set_model/set_device，无需 lux 直写）。两用例用互不重叠的
/// devid/inst 命名空间（hdev1/hdev2），无模型耦合。
async fn register_history_model(client: &lux::EmbeddedClient, devid: &str, inst_id: &str) {
    let _bindings = common::register_devices_as_models(
        client,
        inst_id,
        "dataSim",
        vec![DeviceSpec {
            label: "历史写路径e2e设备".into(),
            value: devid.into(),
            children: vec![
                ChildSpec {
                    label: "归档tag".into(),
                    value: "tagA".into(),
                    r#type: "float".into(),
                    props: Some(TagProps {
                        history: true,
                        ..TagProps::default()
                    }),
                },
                ChildSpec {
                    label: "非归档tag".into(),
                    value: "tagB".into(),
                    r#type: "float".into(),
                    props: Some(TagProps {
                        history: false,
                        ..TagProps::default()
                    }),
                },
            ],
            slave_id: None,
        }],
    )
    .await;
}

/// 取 lux Hash 的 field 列表（HGETALL 空/未建 key 时返回空——轮询友好）。
async fn hash_fields(client: &lux::EmbeddedClient, key: &str) -> Vec<String> {
    match client.execute_value("HGETALL", &[key]).await {
        Ok(lux::EmbeddedValue::Array(a)) => a
            .iter()
            .step_by(2)
            .map(rs_iot::db::str_from_value)
            .collect(),
        _ => Vec::new(),
    }
}

/// 轮询等待条件成立（20ms 间隔），超时返回 false。
async fn poll_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 门禁一预演：hub 送 20 个 Sample（每 Sample 含 tagA+tagB 两值，ts 互异秒）
/// → flush 出口 fan_out 白名单过滤 → writer 落盘 → close 排空 → 同目录重开
/// select_series 断言 series 集合**恰好** = {tagA × hdev1}（tagB 零出现），
/// tagA 唯一 ts 点数 == 20。
#[tokio::test]
async fn history_dataflow_end_to_end() {
    let _serial = serialize();

    // 0. 独立临时目录：lux 与 history 数据目录互不重叠（勿碰仓库 ./data）
    let lux_dir = unique_dir("e2e-lux");
    let hist_dir = unique_dir("e2e-hist");
    let (lux_handle, client) = common::start_lux(lux_dir.to_str().expect("utf8")).await;

    // 1. 建模型（tagA history=true / tagB history=false）+ reload 物化 effective
    register_history_model(&client, "hdev1", "hist_it").await;
    let effective = rs_iot::model::cache::reload(&client)
        .await
        .expect("reload effective 必须成功");
    assert_eq!(
        effective.get("hdev1.tagA").map(|p| p.history),
        Some(true),
        "构造校验：hdev1.tagA 必须在 effective 且 history=true（tag_key 规范化键）"
    );
    assert_eq!(
        effective.get("hdev1.tagB").map(|p| p.history),
        Some(false),
        "构造校验：hdev1.tagB 必须在 effective 且 history=false"
    );

    // 2. 引擎初始化（L2 on）+ writer 句柄
    //    红点：HistoryEngine::handle 尚不存在（E0599）。
    let cfg = HistoryConfig {
        enabled: true,
        data_dir: hist_dir.to_string_lossy().into_owned(),
        ..HistoryConfig::default()
    };
    let mut engine = maybe_init(&cfg, &effective)
        .expect("enabled=true 必须返回 Some")
        .expect("临时目录上引擎初始化必须成功");
    let hist_handle = engine
        .handle()
        .expect("writer 存活时 engine.handle() 必须返回 Some");

    // 3. hub + pipeline：HostContext 携带 history 句柄
    //    红点：history 字段尚不存在（E0560）；HistoryHandle 尚不可 Clone（E0599）。
    let shutdown = CancellationToken::new();
    let ctx = make_ctx(&client, &shutdown, Some(hist_handle.clone()));
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let pipeline_task = spawn_pipeline(ctx, hub_rx);

    // 4. 送 20 个 Sample（ts 自 now 起互异整数秒，防批内去重误伤；tagB 每笔同值）
    let base_ts = now_secs();
    for i in 0..20i64 {
        hub_tx
            .send(sample(
                "hdev1",
                base_ts + i,
                &[
                    ("tagA", SampleValue::Float(20.0 + i as f64)),
                    ("tagB", SampleValue::Float(1.0)),
                ],
            ))
            .await
            .expect("hub send 必须成功");
    }

    // 5. 轮询 written == 20（tagB 不入：flush 出口白名单过滤经 stats 可观测）
    assert!(
        poll_until(Duration::from_secs(5), || {
            hist_handle.stats().written.load(Ordering::SeqCst) == 20
        })
        .await,
        "written 应达 20（超时 5s），实际 written={} sent={} dropped={} skipped={}",
        hist_handle.stats().written.load(Ordering::SeqCst),
        hist_handle.stats().sent.load(Ordering::SeqCst),
        hist_handle.stats().dropped.load(Ordering::SeqCst),
        hist_handle.stats().skipped.load(Ordering::SeqCst),
    );
    assert_eq!(
        hist_handle.stats().skipped.load(Ordering::SeqCst),
        20,
        "tagB 每笔一笔共 20 次白名单外跳过"
    );

    // 6. 收尾：drop hub sender 让 pipeline 收尾（dispatcher 退出 → 级联 worker
    //    排空）→ engine.close 排空关闭（D10：join writer + 释放进程锁）
    drop(hub_tx);
    pipeline_task.await.expect("pipeline join");
    engine.close(Duration::from_secs(15));

    // 7. 同目录重开（close 排空并释放 .tsink.lock 后必须成功）→ 门禁一断言：
    //    series 集合**恰好** = {metric tagA, labels 含 devid=hdev1}，tagB 零出现
    let reopened = build_storage(&cfg, auto_cardinality(&effective))
        .expect("engine.close 排空并释放进程锁后，同目录重开必须成功");
    let series = reopened
        .select_series(&SeriesSelection::new())
        .expect("select_series 必须成功");
    assert_eq!(
        series.len(),
        1,
        "门禁一：series 集合必须恰好一个（tagA×hdev1），实际: {series:?}"
    );
    assert_eq!(
        series[0].name, "tagA",
        "唯一 series 的 metric 必须是 tag 原名 tagA（P1）"
    );
    assert!(
        series[0]
            .labels
            .iter()
            .any(|l| l.name == "devid" && l.value == "hdev1"),
        "series labels 必须含 devid=hdev1，实际: {:?}",
        series[0].labels
    );
    assert!(
        !series
            .iter()
            .any(|s| s.name.contains("tagB") || s.labels.iter().any(|l| l.value.contains("tagB"))),
        "门禁一：白名单外 tagB 零出现（metric 与 labels 均不得含 tagB）"
    );

    // 8. tagA 点数（唯一 ts 计数）== 20（R3：跨批同 ts 双点残留依赖读取端折叠，
    //    此处以唯一 ts 数断言，不受读取端折叠语义影响）
    let pts = reopened
        .select(
            "tagA",
            &[Label::new("devid", "hdev1"), Label::new("hwid", "it-hwid")],
            0,
            i64::MAX,
        )
        .expect("tagA select 必须成功");
    let unique_ts: std::collections::HashSet<i64> = pts.iter().map(|p| p.timestamp).collect();
    assert_eq!(
        unique_ts.len(),
        20,
        "tagA 唯一 ts 点数必须 == 20，实际回读 {} 点（唯一 ts {} 个）",
        pts.len(),
        unique_ts.len()
    );

    reopened.close().expect("重开实例 close");
    common::stop_lux(lux_handle).await;
    let _ = std::fs::remove_dir_all(&hist_dir);
    let _ = std::fs::remove_dir_all(&lux_dir);
}

/// L2 关闭零副作用：enabled=false → maybe_init 返回 None；HostContext.history
/// = None 时数据流照常入库 rtdata（主链路不因历史配置受损）；data_dir 路径
/// （父目录均不存在）保持不存在——无目录、无 .tsink.lock。
#[tokio::test]
async fn history_disabled_zero_side_effect() {
    let _serial = serialize();

    // 0. 独立临时目录（history 目录指向父目录也不存在的全新路径）
    let lux_dir = unique_dir("disabled-lux");
    let hist_dir = unique_dir("disabled-hist").join("history");
    let (lux_handle, client) = common::start_lux(lux_dir.to_str().expect("utf8")).await;

    // 1. 建模型照常（主链路入库不受历史配置影响；本用例不读 effective）
    register_history_model(&client, "hdev2", "hist_off_it").await;

    // 2. L2 关闭（默认 enabled=false）→ maybe_init 必须零副作用返回 None
    let cfg = HistoryConfig {
        data_dir: hist_dir.to_string_lossy().into_owned(),
        ..HistoryConfig::default()
    };
    assert!(
        maybe_init(&cfg, &HashMap::new()).is_none(),
        "enabled=false 必须返回 None"
    );

    // 3. HostContext.history = None + pipeline 照常
    let shutdown = CancellationToken::new();
    let ctx = make_ctx(&client, &shutdown, None);
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let pipeline_task = spawn_pipeline(ctx, hub_rx);

    // 4. 送 3 个 Sample → 轮询主链路落库（rtdata:hdev2 出现 hdev2.tagA 字段）
    let base_ts = now_secs();
    for i in 0..3i64 {
        hub_tx
            .send(sample(
                "hdev2",
                base_ts + i,
                &[
                    ("tagA", SampleValue::Float(3.0 + i as f64)),
                    ("tagB", SampleValue::Float(1.0)),
                ],
            ))
            .await
            .expect("hub send 必须成功");
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let fields = hash_fields(&client, "rtdata:hdev2").await;
        if fields.contains(&"hdev2.tagA".to_string()) && fields.contains(&"hdev2.tagB".to_string())
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "rtdata:hdev2 应出现 hdev2.tagA/hdev2.tagB 字段（主链路不受历史配置影响），实际: {fields:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 5. 零副作用断言：无目录、无 .tsink.lock
    assert!(!hist_dir.exists(), "L2 关闭不得创建 data_dir");
    assert!(
        !hist_dir.join(".tsink.lock").exists(),
        "L2 关闭不得创建进程锁文件"
    );

    // 6. 收尾（hist_dir 本不存在，无需清理）
    drop(hub_tx);
    pipeline_task.await.expect("pipeline join");
    common::stop_lux(lux_handle).await;
    let _ = std::fs::remove_dir_all(&lux_dir);
}
