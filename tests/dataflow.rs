//! 端到端数据流集成测试：真实 lux + identity bootstrap + datasim Source +
//! hub → pipeline → lux rtdata 写入。
//!
//! 覆盖链路：`lib.rs` `run_app_with_shutdown` 中的核心数据流（去掉网络/服务面），
//! 验证南向插件实例化、hub 汇聚、pipeline 落库三段真实可用。

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

/// 构造 datasim 实例配置：period=1s（默认 3s，测试加快）。
fn datasim_inst() -> InstConfig {
    InstConfig {
        inst_id: "sim_it".into(),
        inst_name: "it datasim".into(),
        app_code: PluginName::new("dataSim"),
        app_type: AppType::South,
        auto_start: true,
        app_config: serde_json::json!({ "period": 1 }),
        devices: vec![DeviceSpec {
            label: "demo".into(),
            value: "devA".into(),
            children: vec![
                ChildSpec {
                    label: "温度".into(),
                    value: "temp".into(),
                    r#type: "float".into(),
                },
                ChildSpec {
                    label: "湿度".into(),
                    value: "humidity".into(),
                    r#type: "float".into(),
                },
            ],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    }
}

/// 取 lux Hash 的 field 列表（HGETALL 返回 field1,value1,field2,value2...）。
async fn hash_fields(client: &lux::EmbeddedClient, key: &str) -> Vec<String> {
    match client.execute_value("HGETALL", &[key]).await {
        Ok(lux::EmbeddedValue::Array(a)) => a
            .iter()
            .step_by(2)
            .map(rs_iot::api::str_from_value)
            .collect(),
        other => panic!("expected array for {key}, got {other:?}"),
    }
}

#[tokio::test]
async fn datasim_to_rtdata_end_to_end() {
    let data_dir = common::test_data_dir("dataflow");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;

    // 1. identity bootstrap（幂等建 admin/组）
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    // 2. 组装 hub + InstanceManager + pipeline（passthrough 透传）
    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = InstanceManager::new(ctx.clone(), hub_tx);
    let processor: Arc<dyn Processor> = Arc::new(Passthrough);
    let pipeline_token = shutdown.child_token();
    let pipeline_handle = tokio::spawn(rs_iot::pipeline::run(
        ctx,
        hub_rx,
        processor,
        1, // 测试用单 worker（deterministic，验入库逻辑）
        pipeline_token,
    ));

    // 3. 启动 datasim 实例（南向插件注册表查找 → 独立 task）
    mgr.start_source(datasim_inst())
        .await
        .expect("start datasim");

    // 4. period=1s，等 2.5s 保证至少两轮采集 + pipeline 落库
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // 5. 断言 rtdata:devA 已写入 field（devA.temp / devA.humidity）
    let fields = hash_fields(&client, "rtdata:devA").await;
    assert!(
        fields.contains(&"devA.temp".to_string()),
        "rtdata:devA should contain devA.temp, got {fields:?}"
    );
    assert!(
        fields.contains(&"devA.humidity".to_string()),
        "rtdata:devA should contain devA.humidity, got {fields:?}"
    );

    // 6. 优雅关闭：cancel → pipeline 退出 → stop 实例 → lux SAVE
    shutdown.cancel();
    pipeline_handle.await.expect("pipeline join");
    mgr.stop_all().await;
    common::stop_lux(handle).await;
}
