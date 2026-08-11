//! luaClient 单实例多设备 端到端（CI 内，mock lux + 闲置 tcp endpoint）。
//!
//! M2：collect 嵌套返回 {[devid]={tag=v}} → 多 devid 各自入 rtdata；
//!     ctx:set_values(devid,{tag=v}) 批量 → 一个 Sample 多 tag 入 rtdata。
//! 编排照搬 tests/lua_l2_e2e.rs。

mod common;

use common::test_data_dir;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::plugin::traits::Processor;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// collect 嵌套返回 {[dev1]={a,b}, [dev2]={c}} → rtdata:dev1/dev2 各自入库（多 tag）。
#[tokio::test]
async fn nested_collect_writes_multi_devid_rtdata() {
    let data_dir = test_data_dir("lua-md-nested");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let scripts = std::env::temp_dir().join(format!("lua-mdn-{}", std::process::id()));
    let script_dir = scripts.join("mdn");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        "function collect()\n  return { dev1 = { a = 1, b = 2 }, dev2 = { c = 3 } }\nend\n",
    )
    .expect("write");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: BridgeRegistry::new(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));
    let proc: Arc<dyn Processor> = Arc::new(Passthrough);
    let pt = shutdown.child_token();
    let _pipeline = tokio::spawn(rs_iot::pipeline::run(ctx, hub_rx, proc, 1, pt));

    let inst = InstConfig {
        inst_id: "lua-mdn".into(),
        inst_name: "mdn".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "mdn",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![
            DeviceSpec {
                label: "d1".into(),
                value: "dev1".into(),
                children: vec![],
                slave_id: None,
            },
            DeviceSpec {
                label: "d2".into(),
                value: "dev2".into(),
                children: vec![],
                slave_id: None,
            },
        ],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start");

    tokio::time::sleep(Duration::from_millis(2000)).await;
    let dev1_a = hget_cell(&client, "rtdata:dev1", "dev1.a").await;
    let dev1_b = hget_cell(&client, "rtdata:dev1", "dev1.b").await;
    let dev2_c = hget_cell(&client, "rtdata:dev2", "dev2.c").await;
    assert!(dev1_a.contains('1'), "dev1.a 应入库，得到 '{dev1_a}'");
    assert!(
        dev1_b.contains('2'),
        "dev1.b 应入库（同 Sample 多 tag），得到 '{dev1_b}'"
    );
    assert!(
        dev2_c.contains('3'),
        "dev2.c 应入库（嵌套多设备），得到 '{dev2_c}'"
    );

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

/// ctx:set_values(devid, {tag=v,...}) 批量 → 一个 Sample 多 tag 同时刻入 rtdata。
#[tokio::test]
async fn set_values_writes_multi_tag_rtdata() {
    let data_dir = test_data_dir("lua-md-setvalues");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let scripts = std::env::temp_dir().join(format!("lua-mdsv-{}", std::process::id()));
    let script_dir = scripts.join("mdsv");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        "function collect()\n  ctx:set_values(\"devS\", { temp = 42.5, hum = 60 })\n  return nil\nend\n",
    )
    .expect("write");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: BridgeRegistry::new(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));
    let proc: Arc<dyn Processor> = Arc::new(Passthrough);
    let pt = shutdown.child_token();
    let _pipeline = tokio::spawn(rs_iot::pipeline::run(ctx, hub_rx, proc, 1, pt));

    let inst = InstConfig {
        inst_id: "lua-mdsv".into(),
        inst_name: "mdsv".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "mdsv",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devS".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start");

    tokio::time::sleep(Duration::from_millis(2000)).await;
    let temp = hget_cell(&client, "rtdata:devS", "devS.temp").await;
    let hum = hget_cell(&client, "rtdata:devS", "devS.hum").await;
    assert!(
        temp.contains("42.5"),
        "set_values temp 应入库，得到 '{temp}'"
    );
    assert!(
        hum.contains("60"),
        "set_values hum 应入库（批量多 tag），得到 '{hum}'"
    );

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

/// HGET rtdata cell（`[ts, localTime, value]` JSON 文本）原样返回，断言用 contains。
async fn hget_cell(client: &lux::EmbeddedClient, key: &str, field: &str) -> String {
    use lux::EmbeddedValue;
    match client.execute_value("HGET", &[key, field]).await {
        Ok(EmbeddedValue::Bulk(b)) => String::from_utf8_lossy(&b).to_string(),
        Ok(EmbeddedValue::Simple(s)) => s,
        _ => String::new(),
    }
}
