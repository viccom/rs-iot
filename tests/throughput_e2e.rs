//! 集成测试：N 个 datasim Source → dispatcher+worker → lux,验证入库完整 + worker 数生效。
//! 复用 tests/common（端口 0 + 临时 data_dir），HostContext 构造同 tests/dataflow.rs。

mod common;

use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::plugin::traits::Processor;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{
    AppType, ChildSpec, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample,
};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// datasim 实例：devid=dev{i},50 点,period 1s。
fn datasim_inst(i: u32) -> InstConfig {
    let children: Vec<ChildSpec> = (0..50)
        .map(|j| ChildSpec {
            label: format!("p{j}"),
            value: format!("p{j}"),
            r#type: "float".into(),
        })
        .collect();
    InstConfig {
        inst_id: format!("sim{i}").into(),
        inst_name: format!("it sim{i}"),
        app_code: PluginName::new("dataSim"),
        app_type: AppType::South,
        auto_start: true,
        app_config: serde_json::json!({"period":1}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: format!("dev{i}"),
            children,
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_source_persist_across_workers() {
    let data_dir = common::test_data_dir("throughput");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let metrics = Arc::new(rs_iot::process_metrics::ProcessMetrics::new());
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(metrics.clone()),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: metrics.clone(),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(512);
    let mgr = InstanceManager::new(ctx.clone(), hub_tx);
    let processor: Arc<dyn Processor> = Arc::new(Passthrough);
    let pipeline_token = shutdown.child_token();
    let pipeline_handle = tokio::spawn(rs_iot::pipeline::run(
        ctx,
        hub_rx,
        processor,
        2, // 2 worker,验多 worker 路由
        pipeline_token,
    ));

    // 起 4 实例 × 50 点 = 200 点,经 2 worker 路由
    for i in 0..4u32 {
        mgr.start_source(datasim_inst(i))
            .await
            .unwrap_or_else(|e| panic!("start sim{i}: {e}"));
    }
    tokio::time::sleep(Duration::from_millis(2500)).await; // ≥2 轮采集 + flush

    // 验证每实例 50 点全入库（field 数 == 50）
    for i in 0..4u32 {
        let key = format!("rtdata:dev{i}");
        let n = match client.execute_value("HGETALL", &[&key]).await {
            Ok(lux::EmbeddedValue::Array(a)) => a.len() / 2,
            other => panic!("expected array for {key}, got {other:?}"),
        };
        assert_eq!(n, 50, "dev{i} 应入库 50 点,实际 {n}(路由/入库丢失)");
    }
    // worker 活跃 gauge == 2
    assert_eq!(
        metrics
            .pipeline_workers_active
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(3), pipeline_handle).await;
    mgr.stop_all().await;
    common::stop_lux(handle).await;
}
