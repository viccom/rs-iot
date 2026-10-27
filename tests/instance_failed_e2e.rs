//! M3 自愈可信：实例 task 异常退出 → 真实标 Failed（非 Running）。
//! 验证 supervisor 判定：早退/panic → Failed；正常 stop → Stopped。

mod common;

use async_trait::async_trait;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::{InstState, InstanceManager};
use rs_iot::plugin::registry::SourceEntry;
use rs_iot::plugin::sample_sender::SampleSender;
use rs_iot::plugin::traits::Source;
use rs_iot::types::{AppType, DeviceBinding, HostKind, Hwid, InstConfig, PluginName, Sample};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// mock source：run() 立即返回（模拟配置错误早退）。
struct ImmediateExitSource;
#[async_trait]
impl Source for ImmediateExitSource {
    fn name(&self) -> &'static str {
        "__test_immediate_exit"
    }
    async fn run(
        &self,
        _ctx: HostContext,
        _inst: InstConfig,
        _tx: SampleSender,
        _cmd_rx: mpsc::Receiver<rs_iot::types::CtlMessage>,
        _token: CancellationToken,
    ) {
        // 立即返回（非 token cancel）→ supervisor 应标 Failed
    }
}
inventory::submit!(SourceEntry {
    name: "__test_immediate_exit",
    create: || -> Box<dyn Source> { Box::new(ImmediateExitSource) },
});

/// mock source：run() 内 panic。
struct PanicSource;
#[async_trait]
impl Source for PanicSource {
    fn name(&self) -> &'static str {
        "__test_panic"
    }
    async fn run(
        &self,
        _ctx: HostContext,
        _inst: InstConfig,
        _tx: SampleSender,
        _cmd_rx: mpsc::Receiver<rs_iot::types::CtlMessage>,
        _token: CancellationToken,
    ) {
        panic!("__test_panic boom");
    }
}
inventory::submit!(SourceEntry {
    name: "__test_panic",
    create: || -> Box<dyn Source> { Box::new(PanicSource) },
});

/// mock source：常驻直到 token cancel（模拟正常运行的实例）。
struct ObedientSource;
#[async_trait]
impl Source for ObedientSource {
    fn name(&self) -> &'static str {
        "__test_obedient"
    }
    async fn run(
        &self,
        _ctx: HostContext,
        _inst: InstConfig,
        _tx: SampleSender,
        _cmd_rx: mpsc::Receiver<rs_iot::types::CtlMessage>,
        token: CancellationToken,
    ) {
        token.cancelled().await;
    }
}
inventory::submit!(SourceEntry {
    name: "__test_obedient",
    create: || -> Box<dyn Source> { Box::new(ObedientSource) },
});

fn inst(id: &str, app_code: &str) -> InstConfig {
    InstConfig {
        inst_id: id.into(),
        inst_name: id.into(),
        app_code: PluginName::new(app_code),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({}),
        devices: vec![DeviceBinding {
            devid: id.into(),
            bind: Default::default(),
        }],
        host_kind: HostKind::Builtin,
    }
}

async fn wait_for_state(mgr: &InstanceManager, id: &str, want: InstState) {
    for _ in 0..50 {
        if mgr.status().get(id).copied() == Some(want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "{id} 未在 5s 内进入 {want:?}（当前 {:?})",
        mgr.status().get(id)
    );
}

#[tokio::test]
async fn immediate_exit_marks_failed() {
    let data_dir = common::test_data_dir("instance_failed_imm");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(16);
    let ctx = HostContext {
        lux: client,
        hwid: Hwid::new("test"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: BridgeRegistry::new(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
        history: None,
    };
    let mgr = InstanceManager::new(ctx, hub_tx);

    mgr.start_source(inst("imm", "__test_immediate_exit"))
        .await
        .expect("start");
    wait_for_state(&mgr, "imm", InstState::Failed).await;
    assert_eq!(mgr.status().get("imm").copied(), Some(InstState::Failed));

    common::stop_lux(handle).await;
}

#[tokio::test]
async fn panic_marks_failed() {
    let data_dir = common::test_data_dir("instance_failed_panic");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(16);
    let ctx = HostContext {
        lux: client,
        hwid: Hwid::new("test"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: BridgeRegistry::new(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
        history: None,
    };
    let mgr = InstanceManager::new(ctx, hub_tx);

    mgr.start_source(inst("pan", "__test_panic"))
        .await
        .expect("start");
    wait_for_state(&mgr, "pan", InstState::Failed).await;
    assert_eq!(mgr.status().get("pan").copied(), Some(InstState::Failed));

    common::stop_lux(handle).await;
}

#[tokio::test]
async fn normal_stop_marks_stopped_not_failed() {
    let data_dir = common::test_data_dir("instance_failed_stop");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(16);
    let ctx = HostContext {
        lux: client,
        hwid: Hwid::new("test"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: BridgeRegistry::new(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
        history: None,
    };
    let mgr = InstanceManager::new(ctx, hub_tx);

    mgr.start_source(inst("obe", "__test_obedient"))
        .await
        .expect("start");
    tokio::time::sleep(Duration::from_millis(200)).await; // 确认进入 Running
    assert_eq!(mgr.status().get("obe").copied(), Some(InstState::Running));

    mgr.stop("obe").await.expect("stop");
    assert_eq!(mgr.status().get("obe").copied(), Some(InstState::Stopped));

    common::stop_lux(handle).await;
}

/// 级联删除次生 bug 回归：stop 从未注册的实例（未启动/已删除）必须是无操作——
/// 不得往 states 插入 Stopping/Stopped，否则已删除实例永久残留 /inst/stats。
#[tokio::test]
async fn stop_unregistered_instance_leaves_no_state() {
    let data_dir = common::test_data_dir("instance_stop_unregistered");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(16);
    let ctx = HostContext {
        lux: client,
        hwid: Hwid::new("test"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: BridgeRegistry::new(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
        history: None,
    };
    let mgr = InstanceManager::new(ctx, hub_tx);

    // 从未启动过的实例（如删除级联时对已删实例调 stop）→ stop 后 states 仍为空。
    mgr.stop("ghost").await.expect("stop");
    assert!(
        !mgr.status().contains_key("ghost"),
        "未注册实例 stop 后不得残留状态条目（/inst/stats 会显示已删实例）"
    );

    // 已运行实例 stop 行为不受影响（收敛为 Stopped）。
    mgr.start_source(inst("real", "__test_obedient"))
        .await
        .expect("start");
    tokio::time::sleep(Duration::from_millis(200)).await;
    mgr.stop("real").await.expect("stop");
    assert_eq!(mgr.status().get("real").copied(), Some(InstState::Stopped));

    common::stop_lux(handle).await;
}

// ---------- P1 回归锁：start 的 await 窗口内 stop 不得造成"停不掉的实例" ----------

/// mock source：常驻直到 token cancel（用于模拟 start 已完成注册的实例）。
struct SlowStartSource;
#[async_trait]
impl Source for SlowStartSource {
    fn name(&self) -> &'static str {
        "__test_slow_start"
    }
    async fn run(
        &self,
        _ctx: HostContext,
        _inst: InstConfig,
        _tx: SampleSender,
        _cmd_rx: mpsc::Receiver<rs_iot::types::CtlMessage>,
        token: CancellationToken,
    ) {
        token.cancelled().await;
    }
}
inventory::submit!(SourceEntry {
    name: "__test_slow_start",
    create: || -> Box<dyn Source> { Box::new(SlowStartSource) },
});

/// P1 回归：`claim_starting` 置 Starting 与 `handles.insert` 之间有一个 await
/// （dev_registry::note，每条绑定一次 lux 写）。若此时并发 `stop`：
/// 旧实现看到 states 有条目（Starting）→ 走 stop 流程 → 但 handles 里还没有 token
/// → 置 Stopped 并返回成功；start 随后继续 → insert(Running) + handles.insert
/// → **实例照跑但 stop 已报成功（停不掉）**。
/// 判据：并发 stop 胜出后，实例终态不得为 Running 且 hub 不得再有采样；
/// 若 stop 输给了 start（start 先完成注册），则 stop 必须真正停掉它。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_during_start_never_leaves_zombie() {
    let data_dir = common::test_data_dir("instance_failed_stop_race");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    let (hub_tx, mut hub_rx) = mpsc::channel::<Sample>(256);
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("test"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: BridgeRegistry::new(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
        history: None,
    };
    let mgr = std::sync::Arc::new(InstanceManager::new(ctx, hub_tx));
    // 多设备实例：dev_registry::note 被调用多次 → start 的 await 窗口拉长
    let mut cfg = inst("race", "__test_slow_start");
    cfg.devices = (0..24)
        .map(|i| rs_iot::types::DeviceBinding {
            devid: format!("race-dev-{i}"),
            bind: Default::default(),
        })
        .collect();

    for round in 0..60 {
        // 并发：start 与 stop 同刻起跑；stop 在 start 的 await 窗口内抢占
        let m1 = mgr.clone();
        let m2 = mgr.clone();
        let c1 = {
            let mut c = cfg.clone();
            c.inst_id = format!("race{round}").into();
            c
        };
        let c2 = c1.clone();
        let hs = tokio::spawn(async move { m1.start_source(c1).await });
        // 确定性卡位：等 start 认领 Starting（进入 dev2inst 预登记 await 循环），
        // 此刻 handles 尚无条目——正是竞态窗口
        let mut landed = false;
        for _ in 0..2000 {
            if mgr.status().get(c2.inst_id.as_ref()).copied() == Some(InstState::Starting) {
                landed = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(landed, "第 {round} 轮：未观测到 Starting 窗口");
        let stop_res = m2.stop(c2.inst_id.as_ref()).await;
        let start_res = hs.await;
        assert!(stop_res.is_ok(), "第 {round} 轮：stop 不应报错");
        // 竞态窗口被命中（见上 Starting 卡位）：start 必须放弃注册而不是覆盖终态
        assert!(
            matches!(start_res, Ok(Err(_))),
            "第 {round} 轮：窗口内 start 应放弃注册"
        );
        let _ = (stop_res, start_res);

        // 判据：那次并发 stop 若报告成功，实例必须真的停了——不得留 Running 僵尸
        // （不做二次善后 stop：那会掩盖竞态）
        let id = format!("race{round}");
        let st = mgr.status().get(&id).copied();
        assert_ne!(
            st,
            Some(InstState::Running),
            "第 {round} 轮：并发 stop 后实例仍 Running —— stop 的 await 窗口内认领失效（僵尸实例）"
        );
        // hub 不得再有该实例的采样（用 150ms 窗测：僵尸会持续吐数）
        while hub_rx.try_recv().is_ok() {}
        let got = tokio::time::timeout(Duration::from_millis(150), hub_rx.recv()).await;
        assert!(
            got.is_err(),
            "第 {round} 轮：stop 后 hub 仍有采样（实例未真正停止）"
        );
    }

    common::stop_lux(handle).await;
}
