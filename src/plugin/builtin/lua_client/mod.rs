//! `luaClient`：Lua 5.4 动态脚本采集 Source（MVP：最小采集闭环）。
//!
//! 实例加载 `scripts/<name>/main.lua`，脚本定义 `collect(ctx)`/`on_write(ctx,cmd)`。
//! 详见 `docs/designs/2026-08-06-lua-script-collector-design.md`。

use crate::host::HostContext;
use crate::plugin::lua_core as core_;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use mlua::LuaSerdeExt;
use sha2::Digest;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use core_::config::parse_cfg;
use core_::io_engine::{spawn_io_task, IoBackend, SerialIo, TcpIo, UdpIo};
use core_::loader::SCRIPTS_ROOT;
use core_::script::{load_main, LoadCtx};

/// 运行时设备集（本实例实际上报过数据的 devid，含配置外上报）。
///
/// 主循环（collect）与 ctx API（`set_value`/`set_values`）双出口写入，周期 flush
/// 时经 `StatTracker::extra_insert("runtime_devices")` 落 lux stat:<inst>——前端
/// "实例→实际采集设备"追溯的数据源。std Mutex：insert 瞬时无 await，无死锁面。
pub(crate) type RuntimeDevs = std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>;

/// 把运行时设备集快照写进 tracker extra（每个 flush 点前调用，不留陈旧）。
///
/// BTreeSet 有序 → Vec 稳定，前端 diff 不抖动。
fn sync_runtime_devs(tracker: &mut crate::plugin::stat_tracker::StatTracker, devs: &RuntimeDevs) {
    let snapshot: Vec<String> = devs
        .lock()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    tracker.extra_insert("runtime_devices", serde_json::json!(snapshot));
}

/// 把丢样计数快照写进 tracker extra（每个 flush 点前调用，与 [`sync_runtime_devs`]
/// 并排；Q3 护栏可观测：背压丢了几样一眼可见）。
fn sync_dropped_samples(
    tracker: &mut crate::plugin::stat_tracker::StatTracker,
    dropped: &std::sync::atomic::AtomicU64,
) {
    tracker.extra_insert(
        "dropped_samples",
        serde_json::json!(dropped.load(std::sync::atomic::Ordering::Relaxed)),
    );
}

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
        let lua = match core_::engine::new_vm(&cfg) {
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
            core_::config::Transport::Tcp => Box::new(TcpIo::new(cfg.endpoint.clone(), cfg.recv_idle_ms)),
            core_::config::Transport::Serial => Box::new(SerialIo::new(
                cfg.serial_port.clone().unwrap_or_default(),
                cfg.baud_rate,
                cfg.data_bits,
                cfg.parity,
                cfg.stop_bits,
                cfg.flow_control,
                cfg.recv_idle_ms,
            )),
            core_::config::Transport::Udp => Box::new(UdpIo::new(cfg.endpoint.clone(), cfg.recv_idle_ms)),
        };
        // IO 引擎 + on_data 被动帧 channel（IO task 无在飞 request 时收到的帧 → 主循环）。
        let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(64);
        let io_engine = spawn_io_task(
            backend,
            data_tx,
            inst.inst_id.to_string(),
            ctx.trace_registry.clone(),
        );

        // 4. 注册 ctx API（runtime_devs：L2 上报出口的设备记录，与 collect 出口共用；
        //    dropped：丢样计数，set_value(s) 与 collect 双出口共享，flush 落 stat）。
        let runtime_devs: RuntimeDevs = Default::default();
        let dropped: std::sync::Arc<std::sync::atomic::AtomicU64> = Default::default();
        if let Err(e) = core_::api::register(
            &lua,
            ctx.clone(),
            &inst,
            io_engine.clone(),
            tx.clone(),
            runtime_devs.clone(),
            dropped.clone(),
            cfg.transport.as_str(),
            &scripts_root,
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
        core_::hotload::spawn_watcher(script_dir, reload_tx, token.clone());
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
                _ = flush_ticker.tick() => { sync_runtime_devs(&mut tracker, &runtime_devs); sync_dropped_samples(&mut tracker, &dropped); tracker.flush().await; continue; }
                _ = ticker.tick() => Evt::Tick,
            };
            match evt {
                Evt::Cmd(Some(m)) => {
                    if m.cmd == "write" {
                        match with_budget(
                            "on_write",
                            cfg.callback_timeout_ms,
                            call_on_write(&lua, &m.data),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => tracing::warn!("[luaClient] on_write 失败：{e}"),
                            Err(e) => {
                                // 时长预算超限（方案 a）：warn + stat 计一次；on_write 是
                                // 事件驱动，不进 consecutive_fail（degrade 仅由周期
                                // collect 的 tick_err 路径驱动，见 Evt::Tick）。
                                tracing::warn!("[luaClient] on_write 超时长预算：{e}");
                                tracker.tick_fail("callback timeout");
                            }
                        }
                    }
                }
                Evt::Cmd(None) => break,
                Evt::Data(frame) => {
                    match with_budget(
                        "on_data",
                        cfg.callback_timeout_ms,
                        call_on_data(&lua, &frame),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!("[luaClient] on_data 失败：{e}"),
                        Err(e) => {
                            // 同 on_write：超时计 stat，不进 consecutive_fail。
                            tracing::warn!("[luaClient] on_data 超时长预算：{e}");
                            tracker.tick_fail("callback timeout");
                        }
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
                    match with_budget(
                        "collect",
                        cfg.callback_timeout_ms,
                        call_collect(
                            &lua,
                            inst.devices.first().map(|d| d.value.as_str()).unwrap_or(""),
                        ),
                    )
                    .await
                    {
                        Ok(Ok(samples)) if !samples.is_empty() => {
                            for (devid, values) in samples {
                                if let Ok(mut set) = runtime_devs.lock() {
                                    set.insert(devid.clone());
                                }
                                let sample = Sample {
                                    hwid: ctx.hwid.clone(),
                                    devid,
                                    values,
                                    timestamp: now,
                                };
                                // 防反压（Q3/附录 B 冻结面①）：满 hub 时 2s 后丢样计数，
                                // 控制面（stop/reload/cmd）永不被数据面冻结。
                                match core_::api::send_with_deadline(&tx, sample).await {
                                    core_::api::SendOutcome::Sent => {}
                                    core_::api::SendOutcome::Closed => {
                                        tracing::warn!("[luaClient] hub 关闭，collect 值未上报");
                                    }
                                    core_::api::SendOutcome::Dropped => {
                                        dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        tracing::warn!(
                                            "[luaClient] hub 背压超 2s，collect 丢样（累计 {}）",
                                            dropped.load(std::sync::atomic::Ordering::Relaxed)
                                        );
                                    }
                                }
                            }
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => {
                            tracing::warn!("[luaClient] collect 失败：{e}");
                            tick_err = Some(e.to_string());
                        }
                        Err(e) => {
                            // 时长预算超限（Q3/§4.5-1，spike-1 方案 a）：跳过本轮并并入
                            // tick_err 同一记账路径（tick_fail + consecutive_fail →
                            // fail_threshold → degraded，单次计数不重复）；不重建 VM，
                            // 协程弃留在 await 点，要求脚本幂等。
                            tracing::warn!("[luaClient] collect 超时长预算：{e}");
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
                        sync_runtime_devs(&mut tracker, &runtime_devs);
                        sync_dropped_samples(&mut tracker, &dropped);
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
                        runtime_devs.clone(),
                        dropped.clone(),
                    )
                    .await
                    {
                        Ok(new_lua) => {
                            lua = new_lua;
                            vm_restarts += 1;
                            // 热加载清空运行时设备集：快照语义 = 当前脚本版本的上报
                            // 行为——防脚本改了上报集后旧设备永久残留（下轮 collect 重新积累）。
                            if let Ok(mut s) = runtime_devs.lock() {
                                s.clear();
                            }
                            tracker
                                .extra_insert("vm_restart_count", serde_json::json!(vm_restarts));
                            tracker.extra_insert(
                                "script_version",
                                serde_json::json!(script_version(&scripts_root, &cfg.script)),
                            );
                            consecutive_fail = 0;
                            tracker.set_status("running");
                            sync_runtime_devs(&mut tracker, &runtime_devs);
                            sync_dropped_samples(&mut tracker, &dropped);
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

        // 退出标记 stopped（spec §13 status）。runtime_devices 保留最后一次运行的
        // 设备清单（停止后仍可追溯"它上次采了什么"，诊断价值）。
        tracker.set_status("stopped");
        sync_runtime_devs(&mut tracker, &runtime_devs);
        sync_dropped_samples(&mut tracker, &dropped);
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

/// 单回调时长预算包装（Q3/§4.5-1，T5）：collect/on_write/on_data 每次调用统一包
/// `tokio::time::timeout(callback_timeout_ms)`；Elapsed → 外层 `Err(RuntimeError
/// ("callback timeout: {kind} 超预算 {budget}ms，跳过本轮"))`——消息含回调名与预算，
/// 供 warn 日志与 tick_err/tick_fail 记账。脚本自身的错误（内层 Result 的 Err）原样
/// 透传，与超时严格区分。
///
/// spike-1 方案 a：超时仅跳过本轮 + 计数（连续超限走 collect 的 fail_threshold →
/// degraded），不重建 VM——被 drop 的协程弃留在 await 点，全局态半途，要求脚本幂等。
async fn with_budget<T>(
    kind: &str,
    budget_ms: u64,
    f: impl std::future::Future<Output = T>,
) -> Result<T, mlua::Error> {
    match tokio::time::timeout(std::time::Duration::from_millis(budget_ms), f).await {
        Ok(v) => Ok(v),
        Err(_) => Err(mlua::Error::RuntimeError(format!(
            "callback timeout: {kind} 超预算 {budget_ms}ms，跳过本轮"
        ))),
    }
}

/// 重建 VM：new_vm + load_main + register ctx + dry-run on_init（spec §5.3 热加载）。
///
/// IO engine / ctx_host / 丢样计数复用（不随 VM 替换——dropped 是实例生命周期累计值）。
/// dry-run 跑 on_init（hook 验证不死循环）；collect dry-run 跳过（真 collect 在下 tick，
/// 失败 warn）。失败返回 Err（主循环回退旧 VM）。
// 8 参为 T4 加 dropped 所致——与 api::register 同由（透传其参数集），定点 allow。
#[allow(clippy::too_many_arguments)]
async fn reload_vm(
    cfg: &core_::config::LuaCfg,
    lctx: &LoadCtx,
    ctx_host: HostContext,
    inst: &InstConfig,
    io: core_::io_engine::IoEngine,
    tx: mpsc::Sender<Sample>,
    runtime_devs: RuntimeDevs,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> mlua::Result<mlua::Lua> {
    let lua = core_::engine::new_vm(cfg)?;
    load_main(&lua, lctx)?;
    core_::api::register(
        &lua,
        ctx_host,
        inst,
        io,
        tx,
        runtime_devs,
        dropped,
        cfg.transport.as_str(),
        &lctx.scripts_root,
    )?;
    // dry-run on_init（hook 兜底死循环；失败 → Err → 主循环回退旧 VM）。
    if lua.globals().get::<mlua::Function>("on_init").is_ok() {
        let r: mlua::Result<()> = lua.load("on_init()").eval_async().await;
        r?;
    }
    Ok(lua)
}

/// mlua::Value → SampleValue（对标 transform/rhai.rs 的类型映射）。
pub(crate) fn lua_value_to_sample(v: &mlua::Value) -> Option<SampleValue> {
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
    let path = core_::loader::main_lua_path(scripts_root, name);
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
