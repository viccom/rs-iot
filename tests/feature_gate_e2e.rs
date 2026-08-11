#![cfg(not(feature = "modbus"))]
//! Feature 裁剪优雅降级测试（R1）：关闭 `modbus` feature 后，`modbus_client` 插件
//! 不被 `inventory` 注册，此时建 `app_code="modbusClient"` 的实例，`find_source`
//! 返回 `None` → `start_source` 返回 `Config` 错误（"source plugin not found"），
//! 而非 panic 或静默成功。
//!
//! 与 `s7_e2e.rs` 的 `#![cfg(feature = "s7")]` 对称：s7_e2e 验证**开启**方向
//! （协议插件正常工作），本测试验证**关闭**方向（未注册时优雅降级）。仅在
//! `modbus` feature 关闭时编译运行；CI `minimal` job（`--no-default-features
//! --features lua`）执行它。

mod common;

use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{
    AppType, ChildSpec, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample,
};
use tokio_util::sync::CancellationToken;

/// 关闭 `modbus` feature 时，`modbusClient` 实例应返回 Config 错误（优雅降级），
/// 验证 `find_source` 找不到插件时的错误路径——feature 化后协议被裁剪的正向反馈。
#[tokio::test]
async fn gated_plugin_returns_config_error_when_feature_off() {
    let data_dir = common::test_data_dir("feature-gate");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;

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
    let (hub_tx, _hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = InstanceManager::new(ctx, hub_tx);

    // app_code="modbusClient"，但 modbus feature 关闭 → registry 无此插件 →
    // start_source 在 spawn 前的 find_source 即返回 Config error（不会进到 run/设备解析）。
    let inst = InstConfig {
        inst_id: "gated-it".into(),
        inst_name: "gated modbus".into(),
        app_code: PluginName::new("modbusClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: serde_json::json!({}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devG".into(),
            children: vec![ChildSpec {
                label: "x".into(),
                value: "holding:0:f32".into(),
                r#type: "float".into(),
            }],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };

    let result = mgr.start_source(inst).await;
    assert!(
        result.is_err(),
        "关 modbus feature 时 modbusClient 应返回错误（find_source 找不到），实际: {result:?}"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("source plugin not found"),
        "错误信息应含 'source plugin not found'，实际: {msg}"
    );

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
}
