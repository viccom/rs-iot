//! `luaTransform`：transform 角色 Lua 薄壳（**Source 订阅型**，Q19 三薄插件之三）。
//!
//! C 阶段 T7：transform 不进 pipeline Processor（ADR D1：`Sample` 结构不动是
//! `pipeline.rs:6` 硬约束）——做成 Source trait 订阅型实例，走 InstanceManager
//! 全套（生命周期/stat/热加载）：
//!
//! - **输入面**：`cfg.subscribe` 非空才订 EventBus（Q6：空 = 纯定时）；Rtdata 事件
//!   经 [`config::SubMatcher`] 宿主侧模式过滤 → 自环防护（T8 self-write tracker
//!   窗口过滤——自身输出回流不触发自身）→ `on_rtdata(devid, tag, value)`
//!   逐点回调（每次独立预算，超时/出错跳过该点继续）；非 Rtdata 变体忽略，
//!   Lagged → debug + 继续（broadcast 慢消费覆写旧值语义）。
//! - **节拍面**：`on_timer()` 周期回调（`period` 秒，jitter 相位散布，MissedTick
//!   Skip）。
//! - **输出面**：脚本经 `ctx:set_value` 写派生点 → tx（hub 发送端）→ pipeline
//!   全链路——与 collector 同出口，无新数据通路。
//!
//! 与 luaClient/luaNorth 的结构差异（§5.0「一套沙箱，多种角色」）：
//! - register 先于 load_main（照 luaNorth——脚本顶层能力探针/配置读取可用）；
//! - ApiCtx 角色 Transform（T4）：C1/C3/C5 不注册，`io: None` 不建 IO 引擎，
//!   transport 占位 "none"（不注册不被消费）；
//! - manifest 契约 `@role transform` → on_timer/on_rtdata any-of（T5）。
//!
//! 自环防护（Q6 硬性验收，T8）：[`core_::api::SelfWrites`] per-instance 表——
//! api 层 `push_sample` 记自写点，投递路径窗口（[`core_::api::SELF_LOOP_GUARD_WINDOW`]）
//! 内命中即跳过（跨源同 tag 静默取舍见 const 注释）。

mod config;

use crate::event_bus::EventPayload;
use crate::host::HostContext;
use crate::plugin::lua_core as core_;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use core_::script::{load_main, LoadCtx};

/// 主循环事件（照 luaClient 的 Evt 模式：解耦 select! 与 `&lua`/`&mut lua` 借用，
/// 使 `Evt::Reload` 的 `lua = new_lua`（&mut）与其他分支 `call_*(&lua)`（&）共存）。
enum Evt {
    /// 控制命令（cmd_rx；transform 契约无 on_write——忽略 + debug）。
    Cmd(Option<CtlMessage>),
    /// 热加载信号（脚本目录 watcher 或 cmd_rx "reload"；VM 级替换，失败回退旧 VM）。
    Reload,
    /// on_timer 周期（ticker）。
    Tick,
    /// EventBus 事件（仅 Rtdata 变体消费，其余变体在分派处忽略）。
    Rtdata(EventPayload),
}

/// transform 订阅型薄壳（Source）。
#[derive(Debug, Default)]
pub struct LuaTransform;

#[async_trait]
impl Source for LuaTransform {
    fn name(&self) -> &'static str {
        "luaTransform"
    }

    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        tx: mpsc::Sender<Sample>,
        mut cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    ) {
        // 1. TransformCfg + 订阅匹配器（Q6 宿主过滤；非法配置 warn + 退出）。
        let cfg = match config::parse_transform_cfg(&inst.app_config) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[luaTransform] inst {} 配置错误：{e}", inst.inst_id);
                return;
            }
        };
        let matcher = match config::SubMatcher::new(&cfg.subscribe) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("[luaTransform] inst {} 订阅模式非法：{e}", inst.inst_id);
                return;
            }
        };
        let scripts_root = std::path::Path::new(cfg.scripts_root.as_str()).to_path_buf();
        let lctx = LoadCtx {
            scripts_root: std::sync::Arc::from(scripts_root.as_path()),
            script_name: std::sync::Arc::<str>::from(cfg.script.as_str()),
        };

        // 2. ApiCtx（Transform 角色）：io=None（无传输层，C1 随角色裁剪不注册）；
        //    tx=hub 发送端——脚本 set_value 写派生点经此回 hub → pipeline 全链路。
        //    self_writes 传 Some（Q6 自环防护，T8）：本实例独享一份——api 层
        //    push_sample 发送成功后记 (devid,tag)，本壳在 Rtdata 投递路径消费
        //   （窗口内命中即跳过）；Collector/North 无订阅面恒传 None。
        let runtime_devs: core_::api::RuntimeDevs = Default::default();
        let dropped: std::sync::Arc<std::sync::atomic::AtomicU64> = Default::default();
        let self_writes: core_::api::SelfWrites = Default::default();
        let ac = core_::api::ApiCtx {
            ctx_host: ctx.clone(),
            inst: &inst,
            io: None,
            tx: tx.clone(),
            runtime_devs: runtime_devs.clone(),
            dropped: dropped.clone(),
            self_writes: Some(self_writes.clone()),
            transport: "none",
            scripts_root: &scripts_root,
            role: core_::api::Role::Transform,
        };

        // 3. VM 组装（north 序：register 先于 load_main——顶层能力探针可用）+
        //    manifest dry-run（@role transform any-of 契约，坏契约拒载 warn + return
        //    → supervisor 标 Failed）。
        let mut lua = match build_vm(&cfg, &lctx, &ac) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("[luaTransform] inst {} VM 组装失败：{e}", inst.inst_id);
                return;
            }
        };

        // 4. on_init（若声明，启动即调；照 lua_client 语义，预算包装——挂起型 await
        //    只有预算能切断）。脚本自身失败 = 脚本自己的事：warn 放行（本回调无重试
        //    语义），与 lua_client 启动路径一致。
        if lua.globals().get::<mlua::Function>("on_init").is_ok() {
            match with_budget(
                "on_init",
                cfg.callback_timeout_ms,
                lua.load("on_init()").eval_async::<()>(),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) | Err(e) => {
                    tracing::warn!("[luaTransform] inst {} on_init 失败：{e}", inst.inst_id)
                }
            }
        }

        // 5. 热加载 watcher + on_timer ticker（jitter 相位散布削整点脉冲）+
        //    EventBus 订阅（subscribe 非空才订，Q6：空 = 纯定时不占接收端）。
        let (reload_tx, mut reload_rx) = mpsc::channel::<()>(8);
        core_::hotload::spawn_watcher(
            scripts_root.join(cfg.script.as_str()),
            reload_tx,
            token.clone(),
        );
        let period = std::time::Duration::from_secs(cfg.period.max(1));
        let phase = std::time::Duration::from_millis(crate::plugin::ticker::phase_ms(
            inst.inst_id.as_str(),
            period,
        ));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + phase, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut event_rx = if cfg.subscribe.is_empty() {
            None
        } else {
            Some(ctx.event_tx.subscribe())
        };

        // 6. stat 埋点（spec §13，照 lua_client/north：script_version/vm_restart_count
        //    + transform 专属 rtdata 计数）。
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        tracker.extra_insert(
            "script_version",
            serde_json::json!(core_::script::script_version(&scripts_root, &cfg.script)),
        );
        let mut vm_restarts: u64 = 0;
        // on_rtdata 投递面计数（flush 落 extra：points=成功回调数、skipped=超预算/出错
        // 跳过数——订阅型实例的数据面健康一眼可见；lagged=EventBus 慢消费丢帧数，
        // 复审 P2-4——此前仅 debug 日志，数据面丢失不可观测）。
        let mut rtdata_points: u64 = 0;
        let mut rtdata_skipped: u64 = 0;
        let mut rtdata_lagged: u64 = 0;
        let mut flush_ticker = tokio::time::interval(std::time::Duration::from_secs(
            crate::plugin::stat_tracker::FLUSH_INTERVAL_SECS,
        ));
        tracing::info!(
            "[luaTransform] inst {} started (script={}, period={}s, subscribe={} 条)",
            inst.inst_id,
            cfg.script,
            cfg.period,
            cfg.subscribe.len()
        );

        loop {
            let evt = tokio::select! {
                biased;
                _ = token.cancelled() => break,
                // 控制通道 "reload" 映射 Evt::Reload（Q11：VM 级替换，失败回退旧 VM）。
                msg = cmd_rx.recv() => match msg {
                    Some(m) if m.cmd == "reload" => Evt::Reload,
                    other => Evt::Cmd(other),
                },
                Some(()) = reload_rx.recv() => Evt::Reload, // None（watcher 退出）忽略，不忙循环
                // EventBus（订阅型数据面）：非订阅实例（subscribe 空）分支永久禁用
                //（pending 不完成）；Lagged（慢消费覆写旧值）→ debug + 继续收；
                // Closed → 禁用分支（EventBus 与宿主同生命周期，防御分支）。
                r = async {
                    match event_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => match r {
                    Ok(payload) => Evt::Rtdata(payload),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // 复审 P2-4：丢帧计数入 stat（SSE 侧早有 rs_iot_sse_lagged_total，
                        // 订阅实例侧此前不可观测）——lag 帧数按批累计。
                        rtdata_lagged += n as u64;
                        tracing::debug!(
                            "[luaTransform] inst {} EventBus lagged（丢 {n} 帧继续）",
                            inst.inst_id
                        );
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::debug!("[luaTransform] inst {} EventBus 关闭，停订阅", inst.inst_id);
                        event_rx = None;
                        continue;
                    }
                },
                _ = flush_ticker.tick() => {
                    sync_runtime_devs(&mut tracker, &runtime_devs);
                    sync_transform_stats(&mut tracker, &dropped, rtdata_points, rtdata_skipped, rtdata_lagged);
                    tracker.flush().await;
                    continue;
                }
                _ = ticker.tick() => Evt::Tick,
            };
            match evt {
                Evt::Cmd(Some(m)) => {
                    // transform 回调契约无 on_write（§2.2）——write 等命令忽略 + debug。
                    tracing::debug!(
                        "[luaTransform] inst {} 忽略命令 {}（transform 契约无 on_write）",
                        inst.inst_id,
                        m.cmd
                    );
                }
                Evt::Cmd(None) => break,
                Evt::Tick => {
                    // period=0 = 纯订阅；脚本未定义 on_timer（reload 后可能增减，按次
                    // 检查）也跳过——无活动不记 tick（纯订阅脚本不刷 fail 噪音）。
                    if cfg.period == 0 || lua.globals().get::<mlua::Function>("on_timer").is_err() {
                        continue;
                    }
                    let started = std::time::Instant::now();
                    match with_budget("on_timer", cfg.callback_timeout_ms, call_on_timer(&lua))
                        .await
                    {
                        Ok(Ok(())) => {
                            tracker.tick_ok(started.elapsed().as_millis() as u64);
                        }
                        Ok(Err(e)) | Err(e) => {
                            // 超预算/脚本失败：跳过本轮 + tick_fail（StatTracker 内置
                            // 连续失败阈值驱动 degraded，照 lua_client 记账路径）。
                            tracing::warn!(
                                "[luaTransform] inst {} on_timer 失败：{e}",
                                inst.inst_id
                            );
                            tracker.tick_fail(&e.to_string());
                        }
                    }
                    if tracker.should_flush() {
                        sync_runtime_devs(&mut tracker, &runtime_devs);
                        sync_transform_stats(
                            &mut tracker,
                            &dropped,
                            rtdata_points,
                            rtdata_skipped,
                            rtdata_lagged,
                        );
                        tracker.flush().await;
                    }
                }
                Evt::Rtdata(payload) => {
                    // 只消费 Rtdata 变体（State/Log/Packet 同总线流经——忽略）。
                    let EventPayload::Rtdata { samples, .. } = payload else {
                        continue;
                    };
                    // 批级检查 on_rtdata（reload 后可能增减；纯定时脚本的事件直达忽略）。
                    if lua.globals().get::<mlua::Function>("on_rtdata").is_err() {
                        continue;
                    }
                    // samples 是 Arc<Vec<_>>（复审 P2-4）——fan-out 降为引用计数，
                    // 此处 iter 只读借用，零深克隆。
                    'points: for s in samples.iter() {
                        // tag 双形态（"tag" / "devid.tag"，与 Sample.values 同源）：
                        // 剥本 dev 前缀（照 lua_north pull_rtdata_batch 语义）。
                        let prefix = format!("{}.", s.devid);
                        for (raw_tag, value) in &s.values {
                            // 批内取消可见性（复审 P1-1）：token 只在 select! 臂轮询，
                            // 长批逐点 await 期间不可见——逐点补查，stop 即时生效。
                            if token.is_cancelled() {
                                break 'points;
                            }
                            let tag = raw_tag.strip_prefix(&prefix).unwrap_or(raw_tag.as_str());
                            // Q6 宿主过滤：模式不命中的点不进脚本。
                            if !matcher.matches(&s.devid, tag) {
                                continue;
                            }
                            // T8 自环防护（self-write tracker 窗口过滤）：命中点查
                            // 自写记录，年龄 < SELF_LOOP_GUARD_WINDOW → 跳过——自身
                            // set_value 输出经 hub→pipeline→publish 回流不触发自身
                            // on_rtdata（计数入 rtdata_skipped，与超预算/出错跳过同
                            // 口径）。取舍（五项已定取舍第 2 条）：窗口内其他源对同
                            // (devid,tag) 的并发写入一并静默——per-instance 表只保证
                            // "自身输出不触发自身"，跨源可见性不受保证。
                            let self_written = self_writes
                                .lock()
                                .ok()
                                .and_then(|m| m.get(&(s.devid.clone(), tag.to_string())).copied())
                                .is_some_and(|t| t.elapsed() < core_::api::SELF_LOOP_GUARD_WINDOW);
                            if self_written {
                                rtdata_skipped += 1;
                                continue;
                            }
                            match with_budget(
                                "on_rtdata",
                                cfg.callback_timeout_ms,
                                call_on_rtdata(&lua, &s.devid, tag, value),
                            )
                            .await
                            {
                                Ok(Ok(())) => rtdata_points += 1,
                                Ok(Err(e)) => {
                                    // 脚本即时错误（非挂起）：不产生停摆，逐点继续
                                    //（一点坏不弃整批）。
                                    rtdata_skipped += 1;
                                    tracing::warn!(
                                        "[luaTransform] inst {} on_rtdata 失败（dev={}, tag={}，跳过该点）：{e}",
                                        inst.inst_id,
                                        s.devid,
                                        tag
                                    );
                                }
                                Err(e) => {
                                    // 预算超限（await 型挂起被切断）：**弃批**——跳过
                                    // 批内剩余点（计划取舍 3 原文「跳过剩余点」，与
                                    // collect 超时同语义）。复审 P1-1：此前逐点继续会把
                                    // 整批停摆放大到 命中点数 × callback_timeout_ms，
                                    // stop/reload/cmd 全程不可见。
                                    rtdata_skipped += 1;
                                    tracing::warn!(
                                        "[luaTransform] inst {} on_rtdata 超预算（dev={}, tag={}）：{e}，整批剩余点跳过（弃批）",
                                        inst.inst_id,
                                        s.devid,
                                        tag
                                    );
                                    break 'points;
                                }
                            }
                        }
                    }
                }
                Evt::Reload => match reload_vm(&cfg, &lctx, &ac).await {
                    Ok(new_lua) => {
                        lua = new_lua;
                        vm_restarts += 1;
                        // 热加载清空运行时设备集（快照语义 = 当前脚本版本的上报行为，
                        // 照 lua_client；下轮回调重新积累）。
                        if let Ok(mut set) = runtime_devs.lock() {
                            set.clear();
                        }
                        tracker.extra_insert("vm_restart_count", serde_json::json!(vm_restarts));
                        tracker.extra_insert(
                            "script_version",
                            serde_json::json!(core_::script::script_version(
                                &scripts_root,
                                &cfg.script
                            )),
                        );
                        tracker.set_status("running");
                        sync_runtime_devs(&mut tracker, &runtime_devs);
                        sync_transform_stats(
                            &mut tracker,
                            &dropped,
                            rtdata_points,
                            rtdata_skipped,
                            rtdata_lagged,
                        );
                        tracker.flush().await;
                        tracing::info!("[luaTransform] inst {} 热加载成功", inst.inst_id);
                    }
                    Err(e) => tracing::warn!(
                        "[luaTransform] inst {} 热加载失败，旧 VM 继续：{e}",
                        inst.inst_id
                    ),
                },
            }
        }

        // 退出标记 stopped（spec §13；实例生命周期经由 InstanceManager 全套）。
        tracker.set_status("stopped");
        sync_runtime_devs(&mut tracker, &runtime_devs);
        sync_transform_stats(
            &mut tracker,
            &dropped,
            rtdata_points,
            rtdata_skipped,
            rtdata_lagged,
        );
        tracker.flush().await;
    }
}

/// 组装 VM（启动与 reload 共用）：沙箱 → register（Transform 裁剪，先于 load_main
/// ——顶层能力探针可用）→ load_main → manifest dry-run（@role transform any-of）。
fn build_vm(
    cfg: &config::TransformCfg,
    lctx: &LoadCtx,
    ac: &core_::api::ApiCtx<'_>,
) -> mlua::Result<mlua::Lua> {
    let lua = core_::engine::new_vm_sandboxed(cfg.hook_instr_limit, cfg.mem_limit_mb)?;
    core_::api::register(&lua, ac)?;
    load_main(&lua, lctx)?;
    let stdlib_ver = core_::stdlib_sync::read_index(&lctx.scripts_root).map(|i| i.version);
    let summary = core_::manifest::dry_run_main(
        &lua,
        &lctx.scripts_root,
        &lctx.script_name,
        stdlib_ver.as_deref(),
    )
    .map_err(mlua::Error::RuntimeError)?;
    tracing::info!("[luaTransform] manifest dry-run 通过：{summary}");
    Ok(lua)
}

/// 重建 VM（reload 路径）：[`build_vm`] + dry-run on_init（挂起型 await 预算兜底
/// ——reload 路径挂起会停摆主循环 select!，超限 → Err → 主循环回退旧 VM）。
async fn reload_vm(
    cfg: &config::TransformCfg,
    lctx: &LoadCtx,
    ac: &core_::api::ApiCtx<'_>,
) -> mlua::Result<mlua::Lua> {
    let lua = build_vm(cfg, lctx, ac)?;
    if lua.globals().get::<mlua::Function>("on_init").is_ok() {
        // 双层 Result：外层 = 预算 Elapsed（→ Err → 回退旧 VM），内层 = 脚本错误。
        with_budget(
            "on_init",
            cfg.callback_timeout_ms,
            lua.load("on_init()").eval_async::<()>(),
        )
        .await??;
    }
    Ok(lua)
}

/// 调脚本 `on_timer()`（无参）。未定义由调用方预检（reload 后可能增减）。
async fn call_on_timer(lua: &mlua::Lua) -> mlua::Result<()> {
    let f: mlua::Function = lua.globals().get("on_timer")?;
    let _: mlua::Value = f.call_async(mlua::Value::Nil).await?;
    Ok(())
}

/// 调脚本 `on_rtdata(devid, tag, value)`（devid/tag 字符串，value Lua 标量）。
async fn call_on_rtdata(
    lua: &mlua::Lua,
    devid: &str,
    tag: &str,
    value: &SampleValue,
) -> mlua::Result<()> {
    let f: mlua::Function = lua.globals().get("on_rtdata")?;
    let v = sample_to_lua(lua, value)?;
    let d = lua.create_string(devid)?;
    let t = lua.create_string(tag)?;
    let _: mlua::Value = f.call_async((d, t, v)).await?;
    Ok(())
}

/// SampleValue → Lua 标量（[`core_::api::lua_value_to_sample`] 的逆向：
/// int/float/bool/string 直映射，无 table 嵌套——单点值形状）。
fn sample_to_lua(lua: &mlua::Lua, v: &SampleValue) -> mlua::Result<mlua::Value> {
    Ok(match v {
        SampleValue::Int(i) => mlua::Value::Integer(*i),
        SampleValue::Float(f) => mlua::Value::Number(*f),
        SampleValue::Bool(b) => mlua::Value::Boolean(*b),
        SampleValue::Str(s) => mlua::Value::String(lua.create_string(s.as_bytes())?),
    })
}

/// 单回调时长预算包装（照 lua_client/north with_budget 模式）：on_init/on_timer/
/// on_rtdata 每次调用统一包 `tokio::time::timeout(callback_timeout_ms)`；Elapsed →
/// `Err(RuntimeError("callback timeout: {kind} 超预算 …"))`——消息含回调名与预算，
/// 供 warn 日志与 tick_fail 记账。脚本自身错误原样透传（严格区分）。
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

/// 运行时设备集快照 → tracker extra（照 lua_client：BTreeSet 有序 → Vec 稳定，
/// 前端 diff 不抖动；transform 的派生点上报集经 set_value 记入）。
fn sync_runtime_devs(
    tracker: &mut crate::plugin::stat_tracker::StatTracker,
    devs: &core_::api::RuntimeDevs,
) {
    let snapshot: Vec<String> = devs
        .lock()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    tracker.extra_insert("runtime_devices", serde_json::json!(snapshot));
}

/// transform 投递面计数快照 → tracker extra（每个 flush 点前调用，不留陈旧）。
fn sync_transform_stats(
    tracker: &mut crate::plugin::stat_tracker::StatTracker,
    dropped: &std::sync::atomic::AtomicU64,
    rtdata_points: u64,
    rtdata_skipped: u64,
    rtdata_lagged: u64,
) {
    tracker.extra_insert(
        "dropped_samples",
        serde_json::json!(dropped.load(std::sync::atomic::Ordering::Relaxed)),
    );
    tracker.extra_insert("rtdata_points", serde_json::json!(rtdata_points));
    tracker.extra_insert("rtdata_skipped", serde_json::json!(rtdata_skipped));
    tracker.extra_insert("rtdata_lagged", serde_json::json!(rtdata_lagged));
}

// 静态注册到 inventory（编译期，编进单一二进制）——transform 角色 SourceEntry
// 首个接线者（South 分发；Q19 三薄插件之三）。
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "luaTransform",
        create: || Box::new(LuaTransform),
    }
}
