//! on_data 端到端：mock server 主动推帧 → IO task recv_idle → 主循环 on_data(frame)
//! → 脚本 ctx:kv_set → luakv。验证 M2 欠账补全的被动帧链路（spec §8/§6 on_data）。
//!
//! 绕过未实现的 set_value（L2 欠账）：脚本 on_data 内用 kv_set 落 lux luakv:<inst>，
//! 验证 on_data IO→主循环→回调完整链路。

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
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// mock server 推 0x2A(=42) → 脚本 on_data kv_set('heartbeat', frame:byte(1)) → luakv 命中。
#[tokio::test]
async fn on_data_pushes_to_luakv() {
    let data_dir = test_data_dir("lua-ondata");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    // 脚本：period=0（纯被动，不 collect）+ on_data 把帧首字节转 number kv_set。
    let scripts = std::env::temp_dir().join(format!("lua-ondata-{}", std::process::id()));
    std::fs::create_dir_all(scripts.join("ond")).expect("mkdir");
    std::fs::write(
        scripts.join("ond/main.lua"),
        "function on_data(frame)\n  ctx:kv_set('heartbeat', frame:byte(1))\nend\n",
    )
    .expect("write script");

    // mock server：accept 后 sleep 200ms（等 client 连 + IO task 进 recv_idle）→ 推帧 0x2A=42。
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = s.write_all(&[0x2A]).await;
        tokio::time::sleep(Duration::from_secs(2)).await; // 保连接不断
    });

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
        inst_id: "lua-ond".into(),
        inst_name: "ond".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "ond",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 0,
            "recv_idle_ms": 80,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devOnd".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // 等 server 推帧 + on_data kv_set 落 lux。
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let v = hget_cell(&client, "luakv:lua-ond", "heartbeat").await;
    assert!(
        v.contains("42"),
        "on_data 帧首字节 0x2A=42 应 kv_set heartbeat，得到 '{v}'"
    );

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

/// 取 lux Hash 单 field 的字符串值（HGET）。
async fn hget_cell(client: &lux::EmbeddedClient, key: &str, field: &str) -> String {
    use lux::EmbeddedValue;
    match client.execute_value("HGET", &[key, field]).await {
        Ok(EmbeddedValue::Bulk(b)) => String::from_utf8_lossy(&b).to_string(),
        Ok(EmbeddedValue::Simple(s)) => s,
        _ => String::new(),
    }
}
