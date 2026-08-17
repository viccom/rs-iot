//! M3 自愈可信：实例 task 异常退出 → 真实标 Failed（非 Running）。
//! 验证 supervisor 判定：早退/panic → Failed；正常 stop → Stopped。

mod common;

use async_trait::async_trait;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::{InstState, InstanceManager};
use rs_iot::plugin::registry::SourceEntry;
use rs_iot::plugin::traits::Source;
use rs_iot::types::{AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample};
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
        _tx: mpsc::Sender<Sample>,
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
        _tx: mpsc::Sender<Sample>,
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
        _tx: mpsc::Sender<Sample>,
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
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: id.into(),
            children: vec![],
            slave_id: None,
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
