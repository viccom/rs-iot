//! `luaClient`：Lua 5.4 动态脚本采集 Source（MVP：最小采集闭环）。
//!
//! 实例加载 `scripts/<name>/main.lua`，脚本定义 `collect(ctx)`/`on_write(ctx,cmd)`。
//! 详见 `docs/superpowers/specs/2026-08-06-lua-script-collector-design.md`。

mod api;
mod config;
mod engine;
mod hotload;
mod io_engine;
mod loader;
mod script;

use crate::host::HostContext;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use mlua::LuaSerdeExt;
use sha2::Digest;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use config::parse_cfg;
use io_engine::{spawn_io_task, IoBackend, SerialIo, TcpIo, UdpIo};
use loader::SCRIPTS_ROOT;
use script::{load_main, LoadCtx};

/// 主循环事件（spec §5.4）：解耦 select! 与 `&lua` / `&mut lua` 借用，
/// 使 `Evt::Reload` 的 `lua = new_lua`（&mut）与其他分支 `call_*(&lua)`（&）共存。
enum Evt {
    /// 写命令（cmd_rx）。
    Cmd(Option<CtlMessage>),
    /// on_data 被动帧（data_rx）。
    Data(Vec<u8>),
    /// 采集周期（ticker）。
    Tick,
    /// 热加载信号（reload_rx，notify watcher 触发）。
    Reload,
}

/// Lua 脚本采集 Source（`luaClient`）。
#[derive(Debug, Default)]
pub struct LuaClient;

#[async_trait]
impl Source for LuaClient {
    fn name(&self) -> &'static str {
        "luaClient"
    }

    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        tx: mpsc::Sender<Sample>,
        mut cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    ) {
        // 1. 解析配置。
        let cfg = match parse_cfg(&inst.app_config) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[luaClient] inst {} 配置错误：{e}", inst.inst_id);
                return;
            }
        };
        let scripts_root = inst
            .app_config
            .get("scripts_root")
            .and_then(|v| v.as_str())
            .unwrap_or(SCRIPTS_ROOT);
        let scripts_root = std::path::Path::new(scripts_root).to_path_buf();

        // 2. 建 VM + 加载脚本。
        let lua = match engine::new_vm(&cfg) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("[luaClient] inst {} VM 创建失败：{e}", inst.inst_id);
                return;
            }
        };
        let lctx = LoadCtx {
            scripts_root: std::sync::Arc::from(scripts_root.as_path()),
            script_name: std::sync::Arc::<str>::from(cfg.script.as_str()),
        };
        if let Err(e) = load_main(&lua, &lctx) {
            tracing::warn!("[luaClient] inst {} 脚本加载失败：{e}", inst.inst_id);
            return;
        }

        // 3. IO 引擎（按 transport 选 backend：tcp/serial/udp 三传输，spec §8）。
        let backend: Box<dyn IoBackend> = match cfg.transport {
            config::Transport::Tcp => Box::new(TcpIo::new(cfg.endpoint.clone(), cfg.recv_idle_ms)),
            config::Transport::Serial => Box::new(SerialIo::new(
                cfg.serial_port.clone().unwrap_or_default(),
                cfg.baud_rate,
                cfg.data_bits,
                cfg.parity,
                cfg.stop_bits,
                cfg.flow_control,
                cfg.recv_idle_ms,
            )),
            config::Transport::Udp => Box::new(UdpIo::new(cfg.endpoint.clone(), cfg.recv_idle_ms)),
        };
        // IO 引擎 + on_data 被动帧 channel（IO task 无在飞 request 时收到的帧 → 主循环）。
        let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(64);
        let io_engine = spawn_io_task(backend, data_tx);

        // 4. 注册 ctx API。
        if let Err(e) = api::register(&lua, ctx.clone(), &inst, io_engine.clone()) {
            tracing::warn!("[luaClient] inst {} ctx API 注册失败：{e}", inst.inst_id);
            return;
        }

        // 5. on_init（若脚本定义）。on_init 用全局 ctx（与 collect/on_write 一致，无参）。
        if lua.globals().get::<mlua::Function>("on_init").is_ok() {
            let r: mlua::Result<()> = lua.load("on_init()").eval_async().await;
            if let Err(e) = r {
                tracing::warn!("[luaClient] on_init 失败：{e}");
            }
        }

        // 6. reload channel（notify watcher → Evt::Reload；Task 3 接 watcher）+ 主循环 Evt。
        // _reload_tx 暂不用（Task 3 spawn watcher 改 reload_tx.clone()）；前缀 _ 避 unused。
        let (reload_tx, mut reload_rx) = mpsc::channel::<()>(8);
        // notify watcher 监听 scripts/<name>/ 改动 → reload_tx（热加载，spec §5.3）。
        let script_dir = scripts_root.join(cfg.script.as_str());
        hotload::spawn_watcher(script_dir, reload_tx);
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(cfg.period.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!(
            "[luaClient] inst {} started (script={}, period={}s)",
            inst.inst_id,
            cfg.script,
            cfg.period
        );

        // 状态快照（spec §13）：启动写 running，collect/reload/退出 更新。
        let script_ver = script_version(&scripts_root, &cfg.script);
        let mut stat = crate::db::luastat::LuaStat {
            status: "running".into(),
            collect_count: 0,
            collect_fail_count: 0,
            last_collect_ts: 0,
            last_collect_duration_ms: 0,
            vm_restart_count: 0,
            script_version: script_ver,
            last_err: String::new(),
        };
        let _ = crate::db::luastat::write(&ctx.lux, inst.inst_id.as_ref(), &stat).await;
        // 连续失败计数（内存，不落 lux；仅 stat.status 反映 degraded）。
        let mut consecutive_fail: u32 = 0;

        let mut lua = lua; // 可变（Evt::Reload 替换为新 VM）
        loop {
            let evt = tokio::select! {
                _ = token.cancelled() => break,
                msg = cmd_rx.recv() => Evt::Cmd(msg),
                Some(frame) = data_rx.recv() => Evt::Data(frame),
                _ = reload_rx.recv() => Evt::Reload,
                _ = ticker.tick() => Evt::Tick,
            };
            match evt {
                Evt::Cmd(Some(m)) => {
                    if m.cmd == "write" {
                        if let Err(e) = call_on_write(&lua, &m.data).await {
                            tracing::warn!("[luaClient] on_write 失败：{e}");
                        }
                    }
                }
                Evt::Cmd(None) => break,
                Evt::Data(frame) => {
                    if let Err(e) = call_on_data(&lua, &frame).await {
                        tracing::warn!("[luaClient] on_data 失败：{e}");
                    }
                }
                Evt::Tick => {
                    if cfg.period == 0 {
                        continue;
                    }
                    let started = std::time::Instant::now();
                    let now = chrono::Local::now().timestamp();
                    let mut tick_err: Option<String> = None;
                    for dev in &inst.devices {
                        match call_collect(&lua).await {
                            Ok(Some(values)) if !values.is_empty() => {
                                let sample = Sample {
                                    hwid: ctx.hwid.clone(),
                                    devid: dev.value.clone(),
                                    values,
                                    timestamp: now,
                                };
                                if tx.send(sample).await.is_err() {
                                    break;
                                }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!("[luaClient] collect 失败：{e}");
                                tick_err = Some(e.to_string());
                            }
                        }
                    }
                    // 状态采点（spec §13）：按周期计，任一 collect 失败记 fail 周期。
                    stat.last_collect_ts = now;
                    stat.last_collect_duration_ms = started.elapsed().as_millis() as u64;
                    if let Some(e) = tick_err {
                        stat.collect_fail_count += 1;
                        stat.last_err = e;
                        consecutive_fail = consecutive_fail.saturating_add(1);
                        if consecutive_fail >= cfg.fail_threshold {
                            stat.status = "degraded".into();
                        }
                    } else {
                        stat.collect_count += 1;
                        stat.last_err.clear();
                        consecutive_fail = 0;
                        stat.status = "running".into();
                    }
                    let _ = crate::db::luastat::write(&ctx.lux, inst.inst_id.as_ref(), &stat).await;
                }
                Evt::Reload => {
                    match reload_vm(&cfg, &lctx, ctx.clone(), &inst, io_engine.clone()).await {
                        Ok(new_lua) => {
                            lua = new_lua;
                            stat.vm_restart_count += 1;
                            consecutive_fail = 0;
                            stat.status = "running".into();
                            let _ =
                                crate::db::luastat::write(&ctx.lux, inst.inst_id.as_ref(), &stat)
                                    .await;
                            tracing::info!("[luaClient] inst {} 热加载成功", inst.inst_id);
                        }
                        Err(e) => tracing::warn!(
                            "[luaClient] inst {} 热加载失败，旧 VM 继续：{e}",
                            inst.inst_id
                        ),
                    }
                }
            }
        }

        // 退出标记 stopped（spec §13 status）。
        stat.status = "stopped".into();
        let _ = crate::db::luastat::write(&ctx.lux, inst.inst_id.as_ref(), &stat).await;
    }
}

/// 调脚本 collect(ctx)，返回 values map（key=tag）。脚本未定义 collect 返回 None。
async fn call_collect(lua: &mlua::Lua) -> mlua::Result<Option<BTreeMap<String, SampleValue>>> {
    let f: mlua::Function = match lua.globals().get("collect") {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let result: mlua::Value = f.call_async(mlua::Value::Nil).await?;
    match result {
        mlua::Value::Nil => Ok(None),
        mlua::Value::Table(t) => {
            let mut values = BTreeMap::new();
            for pair in t.pairs::<String, mlua::Value>() {
                let (k, v) = pair?;
                if let Some(sv) = lua_value_to_sample(&v) {
                    values.insert(k, sv);
                }
            }
            Ok(Some(values))
        }
        _ => Ok(None),
    }
}

/// 调脚本 on_write(ctx, cmd)。cmd_data 是 JSON 文本，转 table 传给脚本。
async fn call_on_write(lua: &mlua::Lua, cmd_data: &str) -> mlua::Result<()> {
    let f: mlua::Function = match lua.globals().get("on_write") {
        Ok(f) => f,
        Err(_) => return Ok(()),
    };
    let cmd: mlua::Value = if cmd_data.is_empty() {
        mlua::Value::Nil
    } else {
        let v: serde_json::Value = serde_json::from_str(cmd_data)
            .map_err(|e| mlua::Error::RuntimeError(format!("cmd JSON 解析失败：{e}")))?;
        lua.to_value(&v)?
    };
    let _: mlua::Value = f.call_async(cmd).await?;
    Ok(())
}

/// 调脚本 on_data(frame)。frame 是 IO task 被动收到的字节串。脚本未定义 on_data 则忽略。
async fn call_on_data(lua: &mlua::Lua, frame: &[u8]) -> mlua::Result<()> {
    let f: mlua::Function = match lua.globals().get("on_data") {
        Ok(f) => f,
        Err(_) => return Ok(()),
    };
    let frame_str = lua.create_string(frame)?;
    let _: mlua::Value = f.call_async(frame_str).await?;
    Ok(())
}

/// 重建 VM：new_vm + load_main + register ctx + dry-run on_init（spec §5.3 热加载）。
///
/// IO engine / ctx_host 复用（不随 VM 替换）。dry-run 跑 on_init（hook 验证不死循环）；
/// collect dry-run 跳过（真 collect 在下 tick，失败 warn）。失败返回 Err（主循环回退旧 VM）。
async fn reload_vm(
    cfg: &config::LuaCfg,
    lctx: &LoadCtx,
    ctx_host: HostContext,
    inst: &InstConfig,
    io: io_engine::IoEngine,
) -> mlua::Result<mlua::Lua> {
    let lua = engine::new_vm(cfg)?;
    load_main(&lua, lctx)?;
    api::register(&lua, ctx_host, inst, io)?;
    // dry-run on_init（hook 兜底死循环；失败 → Err → 主循环回退旧 VM）。
    if lua.globals().get::<mlua::Function>("on_init").is_ok() {
        let r: mlua::Result<()> = lua.load("on_init()").eval_async().await;
        r?;
    }
    Ok(lua)
}

/// mlua::Value → SampleValue（对标 transform/rhai.rs 的类型映射）。
fn lua_value_to_sample(v: &mlua::Value) -> Option<SampleValue> {
    match v {
        mlua::Value::Integer(i) => Some(SampleValue::Int(*i)),
        mlua::Value::Number(n) => Some(SampleValue::Float(*n)),
        mlua::Value::Boolean(b) => Some(SampleValue::Bool(*b)),
        mlua::Value::String(s) => Some(SampleValue::Str(s.to_str().ok()?.to_string())),
        _ => None,
    }
}

/// 算脚本版本（main.lua 内容 sha256 前 8 hex，spec §13 `script_version`）。
///
/// 文件读失败返回 `"unknown"`（不阻塞启动；`load_main` 会报真正的加载错误）。
fn script_version(scripts_root: &std::path::Path, name: &str) -> String {
    let path = loader::main_lua_path(scripts_root, name);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let mut h = sha2::Sha256::new();
            h.update(&bytes);
            let sum = h.finalize();
            let mut hex = String::with_capacity(8);
            for b in sum.iter().take(4) {
                hex.push_str(&format!("{:02x}", b));
            }
            hex
        }
        Err(_) => "unknown".into(),
    }
}

// 静态注册到 inventory（编译期，编进单一二进制）
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "luaClient",
        create: || Box::new(LuaClient),
    }
}

#[cfg(test)]
mod tests {
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
            ctl_registry: CtlRegistry::new(),
            bridge_registry: BridgeRegistry::new(),
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
}
