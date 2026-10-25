//! 四门禁 e2e 之门禁一/门禁二（Task 6 验收，总方案 §五 1b 行）：
//!
//! - 门禁一（白名单外零写入）：2 设备 × 各 3 tag 的混合白名单矩阵全量流经
//!   pipeline（hub → flush_batch → fan_out 白名单过滤 → writer → tsink）→
//!   close 排空后同目录重开，`select_series` 的 (tag, devid) 集合**精确等于**
//!   白名单三对——不多不少（集合相等断言，非子集断言）；
//! - 门禁二（死区生效）：同设备 tagD（history=true + deadband=5.0）对值序列
//!   0..=19 只档 |Δ|≥5 的点（PI exception：首点必档、剔行者不更新
//!   last_archived）→ 归档点恰为 {0,5,10,15}；对照组 tagF（history=true 无
//!   死区）全档 20 点。
//!
//! 门禁三（kill 恢复零丢失）见 tests/history_kill_recovery_e2e.rs；门禁四
//! （A/B bench）在 examples/bench_pipeline_e2e.rs 本机手跑，均不在本文件。
//!
//! 夹具口径说明（对齐指令的 written==90 目标）：白名单矩阵为 dev1.tag1/tag2
//! 白、dev1.tag3 非；dev2.tag1 白、dev2.tag2/tag3 非——每设备白 tag 数是
//! 2/1，"每样本只携带本设备 3 tag"的 30 个样本至多产出 45 行；要落满 3 个
//! 白 series 各 30 点（== 指令的 written==90 与"30 样本"口径），每设备各送
//! 30 个 Sample（共 60 个、两设备交错、全局互异秒 ts）。series 集合精确
//! 相等的核心断言不受样本数影响。
#![cfg(feature = "history")]
// SERIALIZE 的 std MutexGuard 刻意跨 await 持有（同 tests/history_dataflow_e2e.rs：
// model::cache 的 effective 是进程级全局快照，本文件两用例的 register 写路径
// 会互相覆盖快照，全局 Mutex 在本 binary 内串行消除覆盖竞态）。
#![allow(clippy::await_holding_lock)]

mod common;

use std::collections::{BTreeMap, BTreeSet, HashSet};
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
use rs_iot::types::{tag_key, ChildSpec, DeviceSpec, Hwid, Sample, SampleValue, TagProps};
use tokio_util::sync::CancellationToken;
use tsink::{Label, SeriesSelection, Value};

/// 同 binary 两用例串行化锁：`model::cache` 的 effective 是进程级全局快照
/// （OnceLock cell），后一用例的 register_devices_as_models（set_model/
/// set_device 写路径）会以自己的 lux 连接触发刷新 → 覆盖全局快照；若前一
/// 用例数据流仍在跑，flush 出口 `fan_out` 读全局 effective 会白名单 miss
/// （flaky）。全局 Mutex 把两用例在本 binary 内串行（集成测试各自独立进程，
/// 锁只作用于本文件）。
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
        "rsiot-hist-write-e2e-{tag}-{}-{nanos}",
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

/// 构造 HostContext（照 tests/history_dataflow_e2e.rs 形态）+ history 句柄。
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

/// 从 tsink Value 提取 F64（类型漂移即 panic，带实际值便于定位）。
fn f64_of(v: &Value) -> f64 {
    match v {
        Value::F64(x) => *x,
        other => panic!("期望 Value::F64，实际: {other:?}"),
    }
}

/// 门禁一：白名单矩阵 2 设备 × 各 3 tag（dev1.tag1/tag2 白、dev1.tag3 非；
/// dev2.tag1 白、dev2.tag2/tag3 非）→ 60 个 Sample（每设备 30、两设备交错、
/// 全局互异秒 ts，每样本携带本设备全部 3 tag）→ 白名单 3 series 各 30 点
/// （written==90）→ close → 同目录重开 → series 集合**精确等于**白名单三对
/// (tag, devid)，非白名单 tag 在 metric/labels 中零出现。
#[tokio::test]
async fn gate1_whitelist_exact_set() {
    let _serial = serialize();

    // 0. 独立临时目录：lux 与 history 数据目录互不重叠（勿碰仓库 ./data）
    let lux_dir = unique_dir("g1-lux");
    let hist_dir = unique_dir("g1-hist");
    let (lux_handle, client) = common::start_lux(lux_dir.to_str().expect("utf8")).await;

    // 1. 建两设备模型（白名单矩阵）+ reload 物化 effective
    let child = |tag: &str, history: bool| ChildSpec {
        label: format!("{tag} 点"),
        value: tag.into(),
        r#type: "float".into(),
        props: Some(TagProps {
            history,
            ..TagProps::default()
        }),
    };
    let _bindings = common::register_devices_as_models(
        &client,
        "g1_gate_it",
        "dataSim",
        vec![
            DeviceSpec {
                label: "门禁一设备1".into(),
                value: "g1dev1".into(),
                children: vec![
                    child("tag1", true),
                    child("tag2", true),
                    child("tag3", false),
                ],
                slave_id: None,
            },
            DeviceSpec {
                label: "门禁一设备2".into(),
                value: "g1dev2".into(),
                children: vec![
                    child("tag1", true),
                    child("tag2", false),
                    child("tag3", false),
                ],
                slave_id: None,
            },
        ],
    )
    .await;
    let effective = rs_iot::model::cache::reload(&client)
        .await
        .expect("reload effective 必须成功");

    // 构造校验：白名单矩阵六卡逐项命中（tag_key 规范化键）
    let matrix = [
        ("g1dev1", "tag1", true),
        ("g1dev1", "tag2", true),
        ("g1dev1", "tag3", false),
        ("g1dev2", "tag1", true),
        ("g1dev2", "tag2", false),
        ("g1dev2", "tag3", false),
    ];
    for (dev, tag, want) in matrix {
        assert_eq!(
            effective.get(&tag_key(dev, tag)).map(|p| p.history),
            Some(want),
            "构造校验：{dev}.{tag} 的 history 必须 == {want}"
        );
    }

    // 2. 引擎初始化（L2 on）+ writer 句柄 + hub/pipeline
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
    let shutdown = CancellationToken::new();
    let ctx = make_ctx(&client, &shutdown, Some(hist_handle.clone()));
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(128);
    let pipeline_task = spawn_pipeline(ctx, hub_rx);

    // 3. 送 60 个 Sample（两设备交错、全局互异秒 ts、每样本带本设备 3 tag）
    let base_ts = now_secs();
    for i in 0..60i64 {
        let devid = if i % 2 == 0 { "g1dev1" } else { "g1dev2" };
        hub_tx
            .send(sample(
                devid,
                base_ts + i,
                &[
                    ("tag1", SampleValue::Float(i as f64)),
                    ("tag2", SampleValue::Float(i as f64 + 0.5)),
                    ("tag3", SampleValue::Float(i as f64 + 0.25)),
                ],
            ))
            .await
            .expect("hub send 必须成功");
    }

    // 4. 轮询 written == 90（30 样本/设备 × 3 白 tag 均摊：g1dev1 两白 +
    //    g1dev2 一白，每白 series 30 点）；非白 3 卡每样本一笔共 90 次 skip
    assert!(
        poll_until(Duration::from_secs(5), || {
            hist_handle.stats().written.load(Ordering::SeqCst) == 90
        })
        .await,
        "written 应达 90（超时 5s），实际 written={} sent={} dropped={} skipped={} errors={}",
        hist_handle.stats().written.load(Ordering::SeqCst),
        hist_handle.stats().sent.load(Ordering::SeqCst),
        hist_handle.stats().dropped.load(Ordering::SeqCst),
        hist_handle.stats().skipped.load(Ordering::SeqCst),
        hist_handle.stats().errors.load(Ordering::SeqCst),
    );
    assert_eq!(
        hist_handle.stats().skipped.load(Ordering::SeqCst),
        90,
        "非白名单 3 卡 × 30 样本/设备 = 90 次白名单外跳过"
    );
    assert_eq!(hist_handle.stats().sent.load(Ordering::SeqCst), 90);
    assert_eq!(hist_handle.stats().dropped.load(Ordering::SeqCst), 0);
    assert_eq!(hist_handle.stats().errors.load(Ordering::SeqCst), 0);

    // 5. 收尾：drop hub sender → pipeline 收尾 → engine.close 排空（D10）
    drop(hub_tx);
    pipeline_task.await.expect("pipeline join");
    engine.close(Duration::from_secs(15));

    // 6. 同目录重开 → 门禁一断言：series 集合**精确等于**白名单三对
    //    {(tag1,g1dev1),(tag2,g1dev1),(tag1,g1dev2)}（集合相等，不多不少）
    let reopened = build_storage(&cfg, auto_cardinality(&effective))
        .expect("engine.close 排空并释放进程锁后，同目录重开必须成功");
    let series = reopened
        .select_series(&SeriesSelection::new())
        .expect("select_series 必须成功");
    let got: BTreeSet<(String, String)> = series
        .iter()
        .map(|ms| {
            let devid = ms
                .labels
                .iter()
                .find(|l| l.name == "devid")
                .expect("series 必带 devid label")
                .value
                .clone();
            (ms.name.clone(), devid)
        })
        .collect();
    let expected: BTreeSet<(String, String)> =
        [("tag1", "g1dev1"), ("tag2", "g1dev1"), ("tag1", "g1dev2")]
            .into_iter()
            .map(|(t, d)| (t.to_string(), d.to_string()))
            .collect();
    assert_eq!(
        got, expected,
        "门禁一：series 集合必须精确等于白名单三对（不多不少），实际: {series:?}"
    );

    // 7. 每白 series 唯一 ts 点数 == 30（盘上数据级复核；3×30=90 与 written 对账）
    for (tag, dev) in [("tag1", "g1dev1"), ("tag2", "g1dev1"), ("tag1", "g1dev2")] {
        let pts = reopened
            .select(
                tag,
                &[Label::new("devid", dev), Label::new("hwid", "it-hwid")],
                0,
                i64::MAX,
            )
            .expect("白 series select 必须成功");
        let unique_ts: HashSet<i64> = pts.iter().map(|p| p.timestamp).collect();
        assert_eq!(
            unique_ts.len(),
            30,
            "白 series {dev}.{tag} 唯一 ts 点数必须 == 30（实际回读 {} 点/唯一 ts {} 个）",
            pts.len(),
            unique_ts.len()
        );
    }

    reopened.close().expect("重开实例 close");
    common::stop_lux(lux_handle).await;
    let _ = std::fs::remove_dir_all(&hist_dir);
    let _ = std::fs::remove_dir_all(&lux_dir);
}

/// 门禁二：同设备 g2dev1 两 tag——tagD（history=true + deadband=5.0）、
/// tagF（history=true 无死区）；20 个 Sample 值序列 0..=19（Float、互异秒
/// ts）→ tagD 归档点恰为 {0,5,10,15}（首点必档；|new−last_archived|≥5 才
/// 档，剔行者不更新 last_archived——PI exception），tagF 全档 20 点。
#[tokio::test]
async fn gate2_deadband_effective() {
    let _serial = serialize();

    // 0. 独立临时目录
    let lux_dir = unique_dir("g2-lux");
    let hist_dir = unique_dir("g2-hist");
    let (lux_handle, client) = common::start_lux(lux_dir.to_str().expect("utf8")).await;

    // 1. 建模型：tagD 带 deadband=5.0、tagF 无死区（两卡均开归档）
    let _bindings = common::register_devices_as_models(
        &client,
        "g2_gate_it",
        "dataSim",
        vec![DeviceSpec {
            label: "门禁二设备".into(),
            value: "g2dev1".into(),
            children: vec![
                ChildSpec {
                    label: "死区tag".into(),
                    value: "tagD".into(),
                    r#type: "float".into(),
                    props: Some(TagProps {
                        history: true,
                        deadband: Some(5.0),
                        ..TagProps::default()
                    }),
                },
                ChildSpec {
                    label: "对照tag".into(),
                    value: "tagF".into(),
                    r#type: "float".into(),
                    props: Some(TagProps {
                        history: true,
                        deadband: None,
                        ..TagProps::default()
                    }),
                },
            ],
            slave_id: None,
        }],
    )
    .await;
    let effective = rs_iot::model::cache::reload(&client)
        .await
        .expect("reload effective 必须成功");

    // 构造校验：deadband 经模型链路（migrate → 模板默认 → effective）透传
    assert_eq!(
        effective
            .get(&tag_key("g2dev1", "tagD"))
            .map(|p| (p.history, p.deadband)),
        Some((true, Some(5.0))),
        "构造校验：g2dev1.tagD 必须 history=true 且 deadband=Some(5.0)"
    );
    assert_eq!(
        effective
            .get(&tag_key("g2dev1", "tagF"))
            .map(|p| (p.history, p.deadband)),
        Some((true, None)),
        "构造校验：g2dev1.tagF 必须 history=true 且无死区"
    );

    // 2. 引擎 + pipeline
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
    let shutdown = CancellationToken::new();
    let ctx = make_ctx(&client, &shutdown, Some(hist_handle.clone()));
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let pipeline_task = spawn_pipeline(ctx, hub_rx);

    // 3. 送 20 个 Sample（值 0..=19，互异秒 ts，两 tag 同值同 ts）
    let base_ts = now_secs();
    for i in 0..20i64 {
        let v = SampleValue::Float(i as f64);
        hub_tx
            .send(sample(
                "g2dev1",
                base_ts + i,
                &[("tagD", v.clone()), ("tagF", v)],
            ))
            .await
            .expect("hub send 必须成功");
    }

    // 4. 轮询：tagD 档 {0,5,10,15} 共 4 点、剔 16 点；tagF 全档 20 点
    assert!(
        poll_until(Duration::from_secs(5), || {
            hist_handle.stats().written.load(Ordering::SeqCst) == 24
                && hist_handle.stats().deadbanded.load(Ordering::SeqCst) == 16
        })
        .await,
        "written 应达 24（tagD 4 + tagF 20）、deadbanded 应达 16（超时 5s），实际 written={} deadbanded={} sent={} skipped={} errors={}",
        hist_handle.stats().written.load(Ordering::SeqCst),
        hist_handle.stats().deadbanded.load(Ordering::SeqCst),
        hist_handle.stats().sent.load(Ordering::SeqCst),
        hist_handle.stats().skipped.load(Ordering::SeqCst),
        hist_handle.stats().errors.load(Ordering::SeqCst),
    );
    assert_eq!(hist_handle.stats().sent.load(Ordering::SeqCst), 40);
    assert_eq!(hist_handle.stats().skipped.load(Ordering::SeqCst), 0);
    assert_eq!(hist_handle.stats().dropped.load(Ordering::SeqCst), 0);
    assert_eq!(hist_handle.stats().errors.load(Ordering::SeqCst), 0);

    // 5. 收尾 + 同目录重开
    drop(hub_tx);
    pipeline_task.await.expect("pipeline join");
    engine.close(Duration::from_secs(15));
    let reopened = build_storage(&cfg, auto_cardinality(&effective))
        .expect("close 排空并释放进程锁后，同目录重开必须成功");

    // 6. 门禁二断言：按 metric + labels(devid) 分别断言精确点数与值集合
    let labels = [Label::new("devid", "g2dev1"), Label::new("hwid", "it-hwid")];

    let d_pts = reopened
        .select("tagD", &labels, 0, i64::MAX)
        .expect("tagD select 必须成功");
    let d_unique_ts: HashSet<i64> = d_pts.iter().map(|p| p.timestamp).collect();
    let mut d_vals: Vec<f64> = d_pts.iter().map(|p| f64_of(&p.value)).collect();
    d_vals.sort_by(|a, b| a.partial_cmp(b).expect("有限 f64 可比较"));
    assert_eq!(
        d_unique_ts.len(),
        4,
        "tagD 归档唯一 ts 数必须 == 4（{{0,5,10,15}}），实际 {} 点/唯一 ts {} 个",
        d_pts.len(),
        d_unique_ts.len()
    );
    assert_eq!(
        d_vals,
        vec![0.0, 5.0, 10.0, 15.0],
        "tagD 归档值集合必须精确为 {{0,5,10,15}}（首点必档；|Δ|≥5 才档；剔行者不更新 last_archived）"
    );

    let f_pts = reopened
        .select("tagF", &labels, 0, i64::MAX)
        .expect("tagF select 必须成功");
    let f_unique_ts: HashSet<i64> = f_pts.iter().map(|p| p.timestamp).collect();
    let mut f_vals: Vec<f64> = f_pts.iter().map(|p| f64_of(&p.value)).collect();
    f_vals.sort_by(|a, b| a.partial_cmp(b).expect("有限 f64 可比较"));
    assert_eq!(
        f_unique_ts.len(),
        20,
        "tagF（无死区对照）唯一 ts 数必须 == 20，实际 {} 点/唯一 ts {} 个",
        f_pts.len(),
        f_unique_ts.len()
    );
    let f_expected: Vec<f64> = (0..20).map(|i| i as f64).collect();
    assert_eq!(f_vals, f_expected, "tagF 值集合必须精确为 0..=19 全档");

    reopened.close().expect("重开实例 close");
    common::stop_lux(lux_handle).await;
    let _ = std::fs::remove_dir_all(&hist_dir);
    let _ = std::fs::remove_dir_all(&lux_dir);
}
