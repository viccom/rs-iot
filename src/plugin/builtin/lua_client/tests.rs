//! luaClient 模块测试（从 mod.rs 拆出，守 mod.rs <500 行）。

use super::LuaClient;
use crate::host::{BridgeRegistry, CtlRegistry, HostContext};
use crate::plugin::registry::find_source;
use crate::plugin::traits::Source;
use crate::types::{AppType, CtlMessage, DeviceSpec, HostKind, Hwid, InstConfig, PluginName};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[test]
fn lua_client_registered() {
    assert!(find_source("luaClient").is_some(), "luaClient 应静态注册");
    assert!(find_source("nope").is_none());
}

#[test]
fn script_version_stable_and_short() {
    let tmp = std::env::temp_dir().join(format!("lua-sv-{}", std::process::id()));
    std::fs::create_dir_all(tmp.join("devA")).unwrap();
    std::fs::write(
        tmp.join("devA/main.lua"),
        "function collect() return { v = 1 } end",
    )
    .unwrap();
    let v1 = super::script_version(&tmp, "devA");
    assert_eq!(v1.len(), 8, "sha256 前 8 hex");
    assert_ne!(v1, "unknown");
    // 同内容 → 同 hash。
    let v2 = super::script_version(&tmp, "devA");
    assert_eq!(v1, v2);
    // 不存在 → unknown。
    assert_eq!(super::script_version(&tmp, "missing"), "unknown");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// call_collect 嵌套返回 {[devid]={tag=v}} → 多个 (devid, map)。
#[tokio::test]
async fn call_collect_nested_returns_multi_devices() {
    let lua = mlua::Lua::new();
    lua.load("function collect() return { dev1 = { a = 1, b = 2 }, dev2 = { c = 3.5 } } end")
        .exec()
        .unwrap();
    let out = super::call_collect(&lua, "fallback")
        .await
        .expect("collect");
    assert_eq!(out.len(), 2, "嵌套应产出 2 个 (devid,map)");
    let by_devid: std::collections::HashMap<
        &str,
        &std::collections::BTreeMap<String, crate::types::SampleValue>,
    > = out.iter().map(|(d, m)| (d.as_str(), m)).collect();
    let m1 = by_devid.get("dev1").expect("dev1 在");
    assert_eq!(m1.len(), 2, "dev1 应含 a,b");
    let m2 = by_devid.get("dev2").expect("dev2 在");
    assert_eq!(m2.len(), 1, "dev2 应含 c");
}

/// call_collect 平铺返回 {tag=v} → 单个 (fallback_devid, map)（老脚本兼容）。
#[tokio::test]
async fn call_collect_flat_uses_fallback_devid() {
    let lua = mlua::Lua::new();
    lua.load("function collect() return { temp = 42.5 } end")
        .exec()
        .unwrap();
    let out = super::call_collect(&lua, "devFirst")
        .await
        .expect("collect");
    assert_eq!(out.len(), 1, "平铺应产出 1 个");
    assert_eq!(out[0].0, "devFirst", "平铺归 fallback_devid");
    assert_eq!(out[0].1.len(), 1, "含 1 tag");
}

/// call_collect 返 nil / 无 collect → 空 Vec（不推 Sample）。
#[tokio::test]
async fn call_collect_nil_or_absent_returns_empty() {
    let lua = mlua::Lua::new();
    lua.load("function collect() return nil end")
        .exec()
        .unwrap();
    let out = super::call_collect(&lua, "d").await.expect("collect nil");
    assert!(out.is_empty(), "nil 返回应空");
    let lua2 = mlua::Lua::new();
    let out2 = super::call_collect(&lua2, "d").await.expect("no collect");
    assert!(out2.is_empty(), "无 collect 应空");
}

#[tokio::test]
async fn collect_pushes_sample_to_hub() {
    let tmp = std::env::temp_dir().join(format!("lua-collect-{}", std::process::id()));
    std::fs::create_dir_all(tmp.join("devA")).unwrap();
    std::fs::write(
        tmp.join("devA/main.lua"),
        "function collect(ctx) return { temp = 42.5 } end",
    )
    .unwrap();

    let (tx, mut rx) = mpsc::channel::<crate::types::Sample>(8);
    let ctx = HostContext {
        lux: mock_lux().await,
        hwid: Hwid::new("it"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            crate::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: BridgeRegistry::new(),
        metrics: std::sync::Arc::new(crate::process_metrics::ProcessMetrics::new()),
        trace_registry: crate::io_trace::TraceRegistry::new(),
        event_tx: crate::event_bus::EventBus::dummy(),
    };
    let inst = InstConfig {
        inst_id: "lua-it".into(),
        inst_name: "it".into(),
        app_code: PluginName::new("luaClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: json!({
            "script":"devA","endpoint":"127.0.0.1:1","period":1,
            "scripts_root": tmp.to_str().unwrap()
        }),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: "devA".into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    };
    let (cmd_tx, cmd_rx) = mpsc::channel::<CtlMessage>(1);
    let token = CancellationToken::new();
    let run_token = token.child_token();
    tokio::spawn(async move {
        LuaClient.run(ctx, inst, tx, cmd_rx, run_token).await;
    });
    let _ = cmd_tx; // 保留 sender，防 cmd_rx.recv() 立即 None 致忙循环

    let s = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
        .await
        .expect("应收到 Sample")
        .expect("Sample 非 None");
    assert_eq!(s.devid, "devA");
    assert!(format!("{:?}", s.values).contains("42.5"));
    token.cancel();
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 🔴 3 回归：watcher（tokio task）随 `token.cancel()` 退出 → `reload_tx` drop →
/// `reload_rx.recv()` 返 `None`。修复（方案 A：`tokio::spawn` + `select!(token.cancelled)`）
/// 下快速退出；回退到 `std::thread` + `loop { sleep }` 时 task 不响应 cancel → `reload_tx`
/// 不 drop → recv 挂起 → 测试超时失败。不依赖 notify 真实事件（只验 token 退出路径）。
#[tokio::test]
async fn watcher_exits_on_cancel() {
    use std::time::Duration;
    let (reload_tx, mut reload_rx) = mpsc::channel::<()>(8);
    let token = CancellationToken::new();
    // 独占临时目录：非共享 temp_dir()，避免并行测试写 temp_dir 触发 notify 事件，
    // 使 reload_rx 收到 Some(()) 而非期望的 None（task 退出 → reload_tx drop）。
    let dir = std::env::temp_dir().join(format!("lua-watcher-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    crate::plugin::lua_core::hotload::spawn_watcher(dir.clone(), reload_tx, token.clone());
    // 等 watcher task 启动（debouncer 创建 + watch 注册）。
    tokio::time::sleep(Duration::from_millis(200)).await;
    token.cancel();
    // 修复：task 退出 → debouncer drop → reload_tx drop → recv() 返 None（<500ms）。
    // bug：task 不响应 cancel → reload_tx 不 drop → recv 挂起 → 500ms 超时。
    let r = tokio::time::timeout(Duration::from_millis(500), reload_rx.recv()).await;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        matches!(r, Ok(None)),
        "watcher 应随 token.cancel 退出（reload_tx drop → recv None），实得 {r:?}"
    );
}

/// T5 单回调时长预算（Q3/§4.5-1）——单元：pending future 在预算内未完成 → Elapsed 分支
/// 映射为 `callback timeout` RuntimeError（含回调名与预算，供 warn/tick_err 记账），
/// 且等满预算才切断；快路径（预算内完成）原样透传返回值。
#[tokio::test]
async fn with_budget_times_out_pending_future() {
    let started = std::time::Instant::now();
    let r: Result<(), mlua::Error> =
        super::with_budget("collect", 200, std::future::pending::<()>()).await;
    let e = r.expect_err("pending future 应在 200ms 预算耗尽时进 Elapsed 分支");
    let msg = e.to_string();
    assert!(msg.contains("callback timeout"), "消息应可辨识超时：{msg}");
    assert!(
        msg.contains("collect") && msg.contains("200"),
        "应含回调名与预算：{msg}"
    );
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(190),
        "应等满预算再切断（实测 {:?}）",
        started.elapsed()
    );
}

/// T5 时长预算——快路径透传：预算内完成的 future 返回值原样传出（不吞不错）。
#[tokio::test]
async fn with_budget_passes_through_completed_future() {
    let r: Result<i32, mlua::Error> = super::with_budget("on_write", 200, async { 7 }).await;
    assert_eq!(r.expect("快路径应透传"), 7);
}

/// T5 集成（spike-1 方案 a）：collect 内 await 永不返回的宿主函数，`with_budget`
/// 以 `callback_timeout_ms`（此处 200ms 配置）切断后 VM 仍可用——重定义 collect 为
/// 快路径再次 `call_collect` 成功出数（= 超时跳过本轮、下一 tick 仍响应；不重建 VM，
/// 协程弃留在 await 点，要求脚本幂等）。
#[tokio::test]
async fn collect_budget_timeout_skips_round_and_vm_stays_usable() {
    let c = crate::plugin::lua_core::config::parse_cfg(
        &json!({"script":"t","endpoint":"x","callback_timeout_ms":200}),
    )
    .unwrap();
    assert_eq!(c.callback_timeout_ms, 200);
    let lua = crate::plugin::lua_core::engine::new_vm(&c).unwrap();
    // 永不完成的宿主函数（spike-1 同款 hang 构造；生产接线即
    // with_budget("collect", cfg.callback_timeout_ms, call_collect(...))）。
    lua.globals()
        .set(
            "hang",
            lua.create_async_function(|_, _: mlua::Value| async move {
                std::future::pending::<mlua::Result<()>>().await
            })
            .unwrap(),
        )
        .unwrap();
    lua.load("function collect() hang() end").exec().unwrap();
    let r = super::with_budget(
        "collect",
        c.callback_timeout_ms,
        super::call_collect(&lua, "d"),
    )
    .await;
    assert!(r.is_err(), "挂起 collect 应在预算处被切断，实得 {r:?}");
    // spike-1 验证点：切断后 VM 可用——下一轮（快路径 collect）正常出数。
    lua.load("function collect() return { v = 1 } end")
        .exec()
        .unwrap();
    let out = super::call_collect(&lua, "d")
        .await
        .expect("切断后下一轮 collect 应正常");
    assert_eq!(out.len(), 1, "下一轮 collect 应正常出数");
}

/// spike-1（设计 §9 R1）：call_async future 被 timeout drop 后，协程停在 await 点，
/// 验证 VM 是否可继续用、脚本全局态是否"半途"。结论决定 T5 方案 a（接受半途态）/b（超时重建 VM）。
#[tokio::test]
async fn spike_timeout_drop_leaves_vm_usable() {
    let mut c = crate::plugin::lua_core::config::parse_cfg(&json!({"script":"t","endpoint":"x"})).unwrap();
    c.hook_instr_limit = 1_000_000;
    let lua = crate::plugin::lua_core::engine::new_vm(&c).unwrap();
    // 永不完成的宿主函数（模拟最慢的 await）。
    // 注：create_async_function 要求 future Output=Result<R>，故 pending 单态化为 mlua::Result<()>。
    lua.globals()
        .set(
            "hang",
            lua.create_async_function(|_, _: mlua::Value| {
                async move { std::future::pending::<mlua::Result<()>>().await }
            })
            .unwrap(),
        )
        .unwrap();
    lua.load("t = {}; function work() t.a = 1; hang(); t.b = 2 end")
        .exec()
        .unwrap();
    let f: mlua::Function = lua.globals().get("work").unwrap();
    // 100ms 超时包裹 call_async → drop future
    let r: Result<mlua::Value, _> = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        f.call_async(mlua::Value::Nil),
    )
    .await
    .map_err(|_| mlua::Error::RuntimeError("timeout".into()))
    .and_then(|x| x);
    assert!(r.is_err(), "应超时");
    // 验证点 1：VM 仍可执行新代码
    let v: i64 = lua.load("return 1 + 1").eval().unwrap();
    assert_eq!(v, 2);
    // 验证点 2：半途态可见（t.a 已写、t.b 未写）——记录实际值
    let a: i64 = lua.load("return t.a").eval().unwrap();
    let has_b: bool = lua.load("return t.b ~= nil").eval().unwrap();
    // 验证点 3：work 可再次调用（协程未锁死 VM）——用短路径版本
    lua.load("function work2() return 7 end").exec().unwrap();
    let f2: mlua::Function = lua.globals().get("work2").unwrap();
    let v2: i64 = f2.call_async(mlua::Value::Nil).await.unwrap();
    assert_eq!(v2, 7);
    println!("SPIKE1: a={a} has_b={has_b}"); // 预期 a=1, has_b=false → 半途态证实
}

async fn mock_lux() -> lux::EmbeddedClient {
    let cfg = crate::config::AppConfig {
        lux: crate::config::LuxConfig {
            port: 0,
            data_dir: std::env::temp_dir()
                .join(format!("lua-run-{}", std::process::id()))
                .to_str()
                .unwrap()
                .to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let lux_cfg = crate::db::build_lux_config(&cfg);
    let handle = lux::run_with_config(lux_cfg).await.expect("lux");
    handle.client()
}
