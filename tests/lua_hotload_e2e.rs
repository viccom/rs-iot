//! 热加载端到端：起实例（脚本返回 v=1）→ 改 main.lua（v=2）→ notify 去抖 → reload_vm
//! → 新 collect 值 2。spec §5.3 / 测试 F（热加载）。
//!
//! 风险（spec §18）：notify Windows 偶有事件丢失 → 本测用较长 sleep 等去抖+reload+新 collect；
//! 若 Windows flaky，CI Linux 应稳定。

mod common;

use common::test_data_dir;
use rs_iot::db::stat;
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::plugin::traits::Processor;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 起实例（v=1）→ 改脚本（v=2）→ notify → reload → 新 collect 值 2 落 rtdata。
#[tokio::test]
async fn hotload_picks_up_script_change() {
    let data_dir = test_data_dir("lua-hotload");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    // 临时脚本目录：v1 返回 v=1。
    let scripts = std::env::temp_dir().join(format!("lua-hl-{}", std::process::id()));
    let script_dir = scripts.join("hl");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        "function collect() return { v = 1 } end\n",
    )
    .expect("write v1");

    // endpoint 闲置端口（collect 不 request，懒连接不触发；spawn_watcher 监听 script_dir）。
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
        inst_id: "lua-hl".into(),
        inst_name: "hl".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "hl",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devHL".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // 等 v1 采集落库（period 1s → ~1.5s 至少一轮 collect）。
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let v1 = hget_cell(&client, "rtdata:devHL", "devHL.v").await;
    assert!(v1.contains('1'), "v1 应采集 v=1，得到 '{v1}'");
    // 等 stat 周期 flush（StatTracker 每 5s 落一次 lux）拿到 v1 的 script_version。
    tokio::time::sleep(Duration::from_millis(5000)).await;
    // 记录 v1 的 script_version（reload 后应变化，验 🟡 4 热加载更新 version）。
    let sv1 = stat::read(&client, "lua-hl")
        .await
        .expect("read stat")
        .expect("stat 存在")
        .extra
        .get("script_version")
        .and_then(|v| v.as_str())
        .expect("script_version 存在")
        .to_string();
    assert_ne!(sv1, "unknown", "v1 脚本存在，version 应是真实 hash");

    // 改脚本：v=2 → notify 去抖（200ms）→ reload_vm（dry-run on_init）→ 新 collect（period 1s）。
    std::fs::write(
        script_dir.join("main.lua"),
        "function collect() return { v = 2 } end\n",
    )
    .expect("write v2");
    // 等去抖 + reload + 至少一轮新 collect（保守 3s：200ms 去抖 + reload + 1s period + 余量）。
    tokio::time::sleep(Duration::from_millis(3000)).await;
    let v2 = hget_cell(&client, "rtdata:devHL", "devHL.v").await;
    assert!(
        v2.contains('2'),
        "热加载后应采集 v=2，得到 '{v2}'（notify Windows 偶丢事件，spec §18）"
    );
    // 热加载后 script_version 应更新（🟡 4 回归）：reload 立即 flush（非周期），
    // 新 main.lua 内容不同 → sha256 不同。
    let sv2 = stat::read(&client, "lua-hl")
        .await
        .expect("read stat")
        .expect("stat 存在")
        .extra
        .get("script_version")
        .and_then(|v| v.as_str())
        .expect("script_version 存在")
        .to_string();
    assert_ne!(
        sv2, sv1,
        "热加载后 script_version 应更新（v1={sv1}，v2={sv2}）"
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
