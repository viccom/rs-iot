//! M4 集成测：lua 脚本经 `ctx:kv_set` 持久化端到端。
//!
//! 验证 Task 1（luakv 数据层）+ Task 2（ctx:kv_set API）的**完整链路**：真实 luaClient
//! task（InstanceManager 拉起）加载脚本 → `collect` 调 `ctx:kv_set` → 落 lux
//! `luakv:<inst_id>`。这是单测未覆盖的端到端面（Task 1 单测只测 luakv 模块直调、
//! Task 2 单测只测 api 层单 VM）。
//!
//! send_command 联动 + delete 清 kv 的端到端由 Task 3/Task 1 单测覆盖
//!（`send_command_routes_to_target` / `remove_clears_instance_kv` 已绿），不重复。

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

/// lua 脚本 `collect` 调 `ctx:kv_set('counter', 42)` → 验 lux `luakv:<inst>` 落库。
///
/// 脚本 collect 不调 `ctx:request`（return 静态表），故 endpoint 用闲置端口也不触发
/// 懒连接（Task 4 的 `TcpIo::new` 懒连接语义）。period=1s 驱动 collect 经 `call_async`
/// 调 kv_set（明确 async 驱动，落库可靠）。
#[tokio::test]
async fn lua_kv_set_persists_to_lux() {
    let data_dir = test_data_dir("lua-m4-kv");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    // 临时脚本目录：collect 调 ctx:kv_set 落库 + 静态 return（不 request，不触发懒连接）。
    let scripts = std::env::temp_dir().join(format!("lua-m4-kv-{}", std::process::id()));
    std::fs::create_dir_all(scripts.join("kvscript")).expect("mkdir");
    std::fs::write(
        scripts.join("kvscript/main.lua"),
        "function collect()\n  ctx:kv_set('counter', 42)\n  return { x = 1 }\nend\n",
    )
    .expect("write script");

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
        inst_id: "lua-kv-it".into(),
        inst_name: "kv it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "kvscript",
            "kind": "tcp",
            "endpoint": "127.0.0.1:1", // 闲置端口：collect 不 request，懒连接不触发
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devKV".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // period=1s，等 1.5s 保证至少一轮 collect → kv_set 落库。
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let v = hget_cell(&client, "luakv:lua-kv-it", "counter").await;
    assert!(
        v.contains("42"),
        "collect 内 ctx:kv_set('counter',42) 应落 lux luakv:lua-kv-it，得到 '{v}'"
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
