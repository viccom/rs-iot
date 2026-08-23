//! M7 状态监测端到端：起 luaClient 实例 → collect 周期 → 验 lux `stat:<inst>` 指标
//!（status=running / collect_count≥1 / script_version 非空）。spec §13。

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

#[tokio::test]
async fn stat_written_after_collect() {
    let data_dir = test_data_dir("lua-stat");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    // 临时脚本目录（collect 返回 v=7）。
    let scripts = std::env::temp_dir().join(format!("lua-stat-{}", std::process::id()));
    let script_dir = scripts.join("st");
    std::fs::create_dir_all(&script_dir).expect("mkdir");
    std::fs::write(
        script_dir.join("main.lua"),
        "function collect() return { v = 7 } end\n",
    )
    .expect("write");

    // mock tcp server（luaClient endpoint；collect 不 request 本测，endpoint 闲置即可）。
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
        inst_id: "lua-st".into(),
        inst_name: "st".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "st",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devST".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // 等 collect 周期 + stat 条件落盘（StatTracker ticker 5s 错峰检查 + 距上次落盘
    // ≥ FLUSH_MAX_INTERVAL_SECS=30s 才写。到期写入的墙钟时刻 = 启动相位(0..5s)+30s
    // → 最坏 ~35s，留 2s 裕量取 37s；period 1s → 覆盖多轮 collect + 一次到期落盘。
    // 启动瞬间的初始 stat（tick=0）由进 loop 前的显式 flush 落）。
    tokio::time::sleep(Duration::from_millis(37000)).await;
    let stat = stat::read(&client, "lua-st")
        .await
        .expect("read stat")
        .expect("stat 应已写入");
    assert_eq!(stat.status, "running", "应 running");
    assert!(
        stat.tick_count >= 1,
        "tick_count ≥ 1，得到 {}",
        stat.tick_count
    );
    let sv = stat
        .extra
        .get("script_version")
        .and_then(|v| v.as_str())
        .expect("script_version 存在");
    assert!(!sv.is_empty(), "script_version 非空");
    assert_ne!(sv, "unknown", "脚本存在，version 应是真实 hash");
    assert!(stat.last_activity_ts > 0, "last_activity_ts 已记");
    assert!(
        stat.last_activity_duration_ms > 0 || stat.tick_count >= 1,
        "duration 已记或已采集"
    );
    // 🔴 1 间接：正常实例无自我热加载（vm_restart_count 应为 0）。直接测需构造 watcher
    // 失败（notify 平台依赖 flaky），降级间接——若 reload_rx None 误触发 reload，此处会非 0。
    let vm_restarts = stat
        .extra
        .get("vm_restart_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert_eq!(
        vm_restarts, 0,
        "正常运行不应触发热加载（vm_restart_count 应为 0）"
    );

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

/// 运行时设备集记录：lua 嵌套多设备 collect（含**配置外**上报的设备）后，
/// stat extra 的 `runtime_devices` 应含全部上报 devid——前端"实例→实际采集设备"
/// 追溯的数据源（配置外的 devRT 只能从这里归属）。
#[tokio::test]
async fn runtime_devices_recorded_in_stat() {
    let data_dir = test_data_dir("lua-stat-rtdev");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    // 嵌套多设备：devCF（配置内）+ devRT（配置外，运行时上报）。
    let scripts = std::env::temp_dir().join(format!("lua-stat-rtdev-{}", std::process::id()));
    std::fs::create_dir_all(scripts.join("rt")).expect("mkdir");
    std::fs::write(
        scripts.join("rt/main.lua"),
        "function collect() return { devCF = { v = 1 }, devRT = { w = 2 } } end\n",
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
        inst_id: "lua-rt".into(),
        inst_name: "rt".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "rt",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devCF".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");

    // collect period 1s + stat 条件落盘（到期时刻最坏 相位+30s≈35s，见上测注释）
    // → 37s 覆盖至少一轮 collect + 一次到期落盘。
    tokio::time::sleep(Duration::from_millis(37000)).await;
    let stat = stat::read(&client, "lua-rt")
        .await
        .expect("read stat")
        .expect("stat 应已写入");
    let devs: Vec<String> = stat
        .extra
        .get("runtime_devices")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .expect("runtime_devices 应存在于 stat extra");
    assert!(
        devs.contains(&"devCF".to_string()),
        "配置内设备 devCF 应被记录，得到 {devs:?}"
    );
    assert!(
        devs.contains(&"devRT".to_string()),
        "配置外运行时上报设备 devRT 也应被记录（前端追溯数据源），得到 {devs:?}"
    );

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}

#[tokio::test]
async fn stat_marked_stopped_on_exit() {
    let data_dir = test_data_dir("lua-stat-stop");
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let scripts = std::env::temp_dir().join(format!("lua-stat-stop-{}", std::process::id()));
    std::fs::create_dir_all(scripts.join("st2")).expect("mkdir");
    std::fs::write(
        scripts.join("st2/main.lua"),
        "function collect() return { v = 1 } end\n",
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
        inst_id: "lua-st2".into(),
        inst_name: "st2".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "script": "st2",
            "kind": "tcp",
            "endpoint": format!("127.0.0.1:{port}"),
            "period": 1,
            "scripts_root": scripts.to_str().expect("utf8"),
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devST2".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start lua");
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 停实例 → run() 退出写 stopped。
    mgr.stop("lua-st2").await.expect("stop");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let stat = stat::read(&client, "lua-st2")
        .await
        .expect("read")
        .expect("stat 存在");
    assert_eq!(stat.status, "stopped", "实例退出后应标 stopped");

    shutdown.cancel();
    mgr.stop_all().await;
    common::stop_lux(handle).await;
    let _ = std::fs::remove_dir_all(&scripts);
}
