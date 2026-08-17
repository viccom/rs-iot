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
        let io_engine = spawn_io_task(
            backend,
            data_tx,
            inst.inst_id.to_string(),
            ctx.trace_registry.clone(),
        );

        // 4. 注册 ctx API。
        if let Err(e) = api::register(
            &lua,
            ctx.clone(),
            &inst,
            io_engine.clone(),
            tx.clone(),
            cfg.transport.as_str(),
        ) {
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

        // 6. reload channel（notify watcher → Evt::Reload）+ 主循环 Evt。
        let (reload_tx, mut reload_rx) = mpsc::channel::<()>(8);
        // notify watcher 监听 scripts/<name>/ 改动 → reload_tx（热加载，spec §5.3）。
        // watcher 随 token 终止（实例 stop 不泄漏线程）；见 hotload::spawn_watcher。
        let script_dir = scripts_root.join(cfg.script.as_str());
        hotload::spawn_watcher(script_dir, reload_tx, token.clone());
        let period = std::time::Duration::from_secs(cfg.period.max(1));
        // jitter 相位：FNV-1a(inst_id) % period，多实例首次 tick 散布，削整点脉冲。
        let phase = std::time::Duration::from_millis(crate::plugin::ticker::phase_ms(
            inst.inst_id.as_str(),
            period,
        ));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + phase, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!(
            "[luaClient] inst {} started (script={}, period={}s)",
            inst.inst_id,
            cfg.script,
            cfg.period
        );
        // 大声失败（CLAUDE.md 规则 10）：v1 重连由 collect period 天然限速，reconnect_secs 不生效。
        if inst.app_config.get("reconnect_secs").is_some() {
            tracing::warn!(
                "[luaClient] inst {}: reconnect_secs 不生效（v1 重连由 collect period 天然限速）",
                inst.inst_id
            );
        }
        // 状态追踪（spec §13）：StatTracker 封装 InstanceStat 计数 + 周期 flush。
        // lua 保留自己的 consecutive_fail + cfg.fail_threshold（可配阈值，默认 10），
        // 用 set_status 覆盖 StatTracker 内部固定阈值（DEGRADED_AFTER_FAILS=3）。
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        tracker.extra_insert(
            "script_version",
            serde_json::json!(script_version(&scripts_root, &cfg.script)),
        );
        let mut vm_restarts: u64 = 0;
        // 连续失败计数（内存，不落 lux；驱动 cfg.fail_threshold 的 degraded 判定）。
        let mut consecutive_fail: u32 = 0;
        let mut flush_ticker = tokio::time::interval(std::time::Duration::from_secs(
            crate::plugin::stat_tracker::FLUSH_INTERVAL_SECS,
        ));

        let mut lua = lua; // 可变（Evt::Reload 替换为新 VM）
        loop {
            let evt = tokio::select! {
                _ = token.cancelled() => break,
                msg = cmd_rx.recv() => Evt::Cmd(msg),
                Some(frame) = data_rx.recv() => Evt::Data(frame),
                Some(()) = reload_rx.recv() => Evt::Reload, // None（watcher 退出/失败）忽略，不忙循环
                _ = flush_ticker.tick() => { tracker.flush().await; continue; }
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
                    // collect 无参不感知设备，只调一次（重复调会使 ctx:request/kv_set 等
                    // 副作用翻倍）；返回值按 spec §3.3 自动判定嵌套多设备 / 平铺单设备。
                    // 平铺模式归口 devices[0]（与 ctx:set_value 的 devices.first() 语义一致，
                    // spec §7 L2「自己当前 devid」）。
                    match call_collect(
                        &lua,
                        inst.devices.first().map(|d| d.value.as_str()).unwrap_or(""),
                    )
                    .await
                    {
                        Ok(samples) if !samples.is_empty() => {
                            for (devid, values) in samples {
                                let sample = Sample {
                                    hwid: ctx.hwid.clone(),
                                    devid,
                                    values,
                                    timestamp: now,
                                };
                                if tx.send(sample).await.is_err() {
                                    tracing::warn!("[luaClient] hub 关闭，collect 值未上报");
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!("[luaClient] collect 失败：{e}");
                            tick_err = Some(e.to_string());
                        }
                    }
                    // 状态采点（spec §13）：按周期计，任一 collect 失败记 fail 周期。
                    // tick_ok/fail 记账 + 更新 last_activity_*；lua 自己的 fail_threshold 判 degraded
                    // 覆盖 StatTracker 内部固定阈值（保留 lua 可配阈值语义）。
                    if let Some(e) = tick_err {
                        tracker.tick_fail(&e);
                        consecutive_fail = consecutive_fail.saturating_add(1);
                        if consecutive_fail >= cfg.fail_threshold {
                            tracker.set_status("degraded");
                        }
                    } else {
                        tracker.tick_ok(started.elapsed().as_millis() as u64);
                        consecutive_fail = 0;
                    }
                    if tracker.should_flush() {
                        tracker.flush().await;
                    }
                }
                Evt::Reload => {
                    match reload_vm(
                        &cfg,
                        &lctx,
                        ctx.clone(),
                        &inst,
                        io_engine.clone(),
                        tx.clone(),
                    )
                    .await
                    {
                        Ok(new_lua) => {
                            lua = new_lua;
                            vm_restarts += 1;
                            tracker
                                .extra_insert("vm_restart_count", serde_json::json!(vm_restarts));
                            tracker.extra_insert(
                                "script_version",
                                serde_json::json!(script_version(&scripts_root, &cfg.script)),
                            );
                            consecutive_fail = 0;
                            tracker.set_status("running");
                            tracker.flush().await;
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
        tracker.set_status("stopped");
        tracker.flush().await;
    }
}

/// 调脚本 `collect()`，返回 `(devid, values)` 列表（多设备支持）。
///
/// 返回值 table 自动判定（spec §3.3，零歧义——`SampleValue` 只含标量）：
/// - 外层 table 的 value 是 **table** → 嵌套多设备：`{[devid]={tag=v,...}}`
///   → 每个内层 table 解析成一个 `(devid, map)`。
/// - 外层 table 的 value 是 **标量** → 平铺单设备（老脚本兼容）：
///   → 整个 table 解析成 `(fallback_devid, map)`。
/// - 嵌套中混入标量（脚本 bug）→ warn 跳过该项。
///
/// `fallback_devid` 为平铺模式的归口 devid（调用方传 `devices[0].value`）。
/// 脚本未定义 collect / 返 nil / 非 table → 空 Vec（不推 Sample）。
async fn call_collect(
    lua: &mlua::Lua,
    fallback_devid: &str,
) -> mlua::Result<Vec<(String, BTreeMap<String, SampleValue>)>> {
    let f: mlua::Function = match lua.globals().get("collect") {
        Ok(f) => f,
        Err(_) => return Ok(vec![]),
    };
    let result: mlua::Value = f.call_async(mlua::Value::Nil).await?;
    let outer = match result {
        mlua::Value::Nil => return Ok(vec![]),
        mlua::Value::Table(t) => t,
        _ => return Ok(vec![]),
    };
    // 一遍检测：外层任一 value 是 table → 嵌套模式（table.clone() 廉价，RefCounted）。
    // 解析失败的 pair 静默跳过（仅探测模式，错误留给下面的正式遍历报）。
    let mut nested = false;
    for (_, v) in outer.clone().pairs::<String, mlua::Value>().flatten() {
        if matches!(v, mlua::Value::Table(_)) {
            nested = true;
            break;
        }
    }
    let mut out: Vec<(String, BTreeMap<String, SampleValue>)> = Vec::new();
    if nested {
        for pair in outer.pairs::<String, mlua::Value>() {
            let (devid, v) = pair?;
            match v {
                mlua::Value::Table(inner) => {
                    let map = parse_table_to_sample_map(&inner)?;
                    if !map.is_empty() {
                        out.push((devid, map));
                    }
                }
                _ => tracing::warn!(
                    "[luaClient] collect 嵌套返回含非 table 值（key={devid}），已跳过"
                ),
            }
        }
    } else {
        let map = parse_table_to_sample_map(&outer)?;
        if !map.is_empty() {
            out.push((fallback_devid.to_string(), map));
        }
    }
    Ok(out)
}

/// mlua table `{tag = value}` → `BTreeMap<String, SampleValue>`（标量 value；非标量跳过）。
fn parse_table_to_sample_map(t: &mlua::Table) -> mlua::Result<BTreeMap<String, SampleValue>> {
    let mut values = BTreeMap::new();
    for pair in t.pairs::<String, mlua::Value>() {
        let (k, v) = pair?;
        if let Some(sv) = lua_value_to_sample(&v) {
            values.insert(k, sv);
        }
    }
    Ok(values)
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
    tx: mpsc::Sender<Sample>,
) -> mlua::Result<mlua::Lua> {
    let lua = engine::new_vm(cfg)?;
    load_main(&lua, lctx)?;
    api::register(&lua, ctx_host, inst, io, tx, cfg.transport.as_str())?;
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
mod tests;
