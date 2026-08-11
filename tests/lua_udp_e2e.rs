//! M8a udp 传输端到端：mock udp server（回 Modbus-like 响应）+ luaClient Udp 实例
//! + collect → rtdata 验值。spec §8（三传输）+ §16（udp）。

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
async fn udp_collect_writes_rtdata() {
    let data_dir = test_data_dir("lua-udp");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    // mock udp server：收任 frame 回 "[0x00, 0x2A]"（脚本解析 byte(2)=42 作为 v）。
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = server.local_addr().expect("addr");
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        loop {
            let (n, peer) = match server.recv_from(&mut buf).await {
                Ok(x) => x,
                Err(_) => break,
            };
            // 回两字节：0x00 + 0x2A（42）。
            let resp = if n > 0 { vec![0x00, 0x2A] } else { vec![] };
            let _ = server.send_to(&resp, peer).await;
        }
    });

    // 临时脚本：collect 发一帧，解析响应 byte(2) 为 v。
    let scripts = std::env::temp_dir().join(format!("lua-udp-{}", std::process::id()));
    let script_dir = scripts.join("u");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        "function collect()\n\
         local r = ctx:request(\"\\x01\", 300)\n\
         if not r or #r < 2 then return nil end\n\
         return { v = r:byte(2) }\n\
         end\n",
    )
    .expect("write");

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
        inst_id: "lua-udp".into(),
        inst_name: "u".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "u",
            "kind": "udp",
            "endpoint": addr.to_string(),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devU".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // 等 collect 周期 → rtdata 落 v=42。
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let v = hget_cell(&client, "rtdata:devU", "devU.v").await;
    assert!(v.contains("42"), "udp collect 应得 v=42，得到 '{v}'");

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

async fn hget_cell(client: &lux::EmbeddedClient, key: &str, field: &str) -> String {
    use lux::EmbeddedValue;
    match client.execute_value("HGET", &[key, field]).await {
        Ok(EmbeddedValue::Bulk(b)) => String::from_utf8_lossy(&b).to_string(),
        Ok(EmbeddedValue::Simple(s)) => s,
        _ => String::new(),
    }
}
