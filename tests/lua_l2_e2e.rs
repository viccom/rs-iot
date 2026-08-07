//! M8b L2 数据 API 端到端：`collect` 内 `ctx:set_value` → hub → pipeline → lux rtdata。
//! 验 set_value 推 Sample 经数据上行总线入库（spec §7 L2）。Plan 8 Task 4。

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

#[tokio::test]
async fn l2_set_value_writes_rtdata() {
    let data_dir = test_data_dir("lua-l2");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    // 脚本：collect 调 set_value("temp", 42.5)（devid=devices[0]=devL2），返 nil
    // （set_value 已推 hub，不走 collect table 路径）。
    let scripts = std::env::temp_dir().join(format!("lua-l2-{}", std::process::id()));
    let script_dir = scripts.join("l2");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        "function collect()\n    ctx:set_value(\"temp\", 42.5)\n    return nil\nend\n",
    )
    .expect("write");

    // mock tcp endpoint（collect 不 request，endpoint 闲置；bind :0 拿端口避免占用固定口）。
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
        ctl_registry: CtlRegistry::new(),
        bridge_registry: BridgeRegistry::new(),
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));
    let proc: Arc<dyn Processor> = Arc::new(Passthrough);
    let pt = shutdown.child_token();
    let _pipeline = tokio::spawn(rs_iot::pipeline::run(ctx, hub_rx, proc, pt));

    let inst = InstConfig {
        inst_id: "lua-l2".into(),
        inst_name: "l2".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "l2",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devL2".into(),
            children: vec![],
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start");

    // 等 collect(period=1s) → set_value → hub → pipeline → rtdata。
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let v = hget_cell(&client, "rtdata:devL2", "devL2.temp").await;
    assert!(
        v.contains("42.5"),
        "set_value 应落 rtdata:devL2 devL2.temp=42.5，得到 '{v}'"
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
