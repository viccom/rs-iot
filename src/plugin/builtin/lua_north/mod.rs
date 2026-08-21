//! `luaNorth`：mqtt-north 角色 Lua 薄壳（**Sink trait 首个接线者**，Q19/R4）。
//!
//! 阶段 B T6：实例生命周期（InstanceManager 全套）+ VM 组装（沙箱/loader/能力包
//! 裁剪）+ cfg 全局表（Q5）+ manifest 多角色契约（`@role mqtt-north` → auth+up
//! 必需）+ 热加载 + stat 埋点。
//!
//! T7（本 task）：mqtt-north 回调契约填肉——mqtt task（网络 IO 独占，见
//! [`contract`]）+ 连接序列（will/auth → MqttOptions，**重连必重调 auth**，Q5）+
//! BatchTick 的 lux pull 收拢批量 → `up(batch)` → 逐条投递 + down/on_connack/
//! on_disconnect 回调 + stat（publish_ok/publish_fail/reconnect_count）。
//!
//! T8：[`compat`]——旧 `[mqtt]` 配置 → 默认实例 `mqtt-pub-default` 的无损迁移
//! （内置 mqttPub 兼容脚本 + R5 单通道锁），编排层接线见 `lib.rs`。
//!
//! 与 luaClient 的结构差异（§5.0「一套沙箱，多种角色」）：
//! - register 先于 load_main（collector 相反）——mqtt-north 脚本顶层即可用
//!   `ctx`/`cfg`（能力探针/配置读取）；
//! - ApiCtx 角色 North：C1（request/transport）不注册，`io: None` 不建 IO 引擎
//!   （§5.2 矩阵 mqtt-north 列）；
//! - 数据收拢走 lux pull 快照批量（取舍 1，Q6）：Sink `rx` 恒闲置但语义正确
//!   消费——收到即入批缓冲，随 BatchTick flush 一并交 up()。

pub(crate) mod compat;
mod config;
mod contract;

use crate::host::HostContext;
use crate::plugin::lua_core as core_;
use crate::plugin::traits::Sink;
use crate::types::{CtlMessage, InstConfig, Sample};
use async_trait::async_trait;
use mlua::LuaSerdeExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use core_::loader::SCRIPTS_ROOT;
use core_::script::{load_main, LoadCtx};

/// 主循环事件（照 luaClient 的 Evt 模式：解耦 select! 与 `&lua`/`&mut lua` 借用）。
enum Evt {
    /// 控制命令（cmd_rx；mqtt-north 契约无 on_write——忽略 + debug，§5.1）。
    Cmd(Option<CtlMessage>),
    /// 样本（rx；取舍 1：入批缓冲，BatchTick flush 消费）。
    Sample(Sample),
    /// 批量周期（batch_max_secs；lux pull 收拢 + up() 发布点）。
    BatchTick,
    /// 热加载信号（脚本目录 watcher 或 cmd_rx "reload"；VM 级替换，失败回退）。
    Reload,
    /// mqtt task 事件（ConnAck/断连/下行/发布计数）。
    Mqtt(contract::MqttEvt),
    /// 重连退避到期（断连后等一个 period，重走连接序列——Q5 重连必重调 auth）。
    ReconnectNow,
}

/// mqtt-north 北向薄壳（Sink）。
#[derive(Debug, Default)]
pub struct LuaNorth;

/// 注入全局 `cfg` 表（Q5「宿主另注入全局 cfg 表」/ `auth(cfg)` 参数）：
/// NorthCfg 有效子集（client_id 缺省已物化；password 保留 `secret:` 引用原样——
/// 解析收敛在连接前一刻，明文不长期驻留 VM globals）。
fn inject_cfg_table(
    lua: &mlua::Lua,
    cfg: &config::NorthCfg,
    ac: &core_::api::ApiCtx<'_>,
) -> mlua::Result<()> {
    let v = cfg.to_script_cfg(ac.ctx_host.hwid.as_str(), ac.inst.inst_id.as_str());
    let t: mlua::Value = lua.to_value(&v)?;
    lua.globals().set("cfg", t)
}

/// 重建 VM（north 版 reload）：沙箱 VM → register（North 裁剪）→ cfg 表 →
/// load_main → manifest dry-run（多角色契约，坏契约 → Err → 主循环回退旧 VM）。
///
/// 契约回调（on_init 等）目前无 north 侧调用点（§5.1 north 契约不含 on_init）；
/// 加载期指令 hook 兜底纯计算死循环（§9）。新 VM 就位后契约回调（auth/up/down…）
/// 自动从新 VM 调用（主循环只持 `lua` 句柄）；MQTT 连接不随 reload 重建（Q5 语义
/// 只锁「重连必重调 auth」，reload 换 VM 后下次重连自然用新 VM 的 auth）。
fn reload_vm(
    cfg: &config::NorthCfg,
    lctx: &LoadCtx,
    ac: &core_::api::ApiCtx<'_>,
) -> mlua::Result<mlua::Lua> {
    let lua = core_::engine::new_vm_sandboxed(cfg.hook_instr_limit, cfg.mem_limit_mb)?;
    core_::api::register(&lua, ac)?;
    inject_cfg_table(&lua, cfg, ac)?;
    load_main(&lua, lctx)?;
    let stdlib_ver = core_::stdlib_sync::read_index(&lctx.scripts_root).map(|i| i.version);
    let summary = core_::manifest::dry_run_main(
        &lua,
        &lctx.scripts_root,
        &lctx.script_name,
        stdlib_ver.as_deref(),
    )
    .map_err(mlua::Error::RuntimeError)?;
    tracing::info!("[luaNorth] 热加载 manifest dry-run 通过：{summary}");
    Ok(lua)
}

#[async_trait]
impl Sink for LuaNorth {
    fn name(&self) -> &'static str {
        "luaNorth"
    }

    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        mut rx: mpsc::Receiver<Sample>,
        mut cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    ) {
        // 1. NorthCfg（broker/批量节奏；无传输项——MQTT 连接层在宿主）。
        let cfg = match config::parse_north_cfg(&inst.app_config) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[luaNorth] inst {} 配置错误：{e}", inst.inst_id);
                return;
            }
        };
        let scripts_root = inst
            .app_config
            .get("scripts_root")
            .and_then(|v| v.as_str())
            .unwrap_or(SCRIPTS_ROOT);
        let scripts_root = std::path::Path::new(scripts_root).to_path_buf();

        // 2. ApiCtx（North 角色）：io=None（无传输层，C1 随角色裁剪不注册）。
        //    tx 为黑洞缓冲通道（T6 偏离点 3 定案）：set_value(s) 在 north 角色照常
        //    注册（保留函数面——脚本无差别可用），但 north 是 Sink（数据消费者）写
        //    hub 概念上不成立——接收端保活不消费 = send 成功即弃（零噪音；缓冲打满
        //    才 dropped 计数，语义同慢消费者，容量 1024 实际打不满）。
        let (stub_tx, _stub_rx) = mpsc::channel::<Sample>(1024);
        // 告警护栏（Q12 T4）：fire 频控 + 抖动表 + 频控计数（flush 落 stat）。
        let alarm: core_::api::AlarmStats = Default::default();
        let ac = core_::api::ApiCtx {
            ctx_host: ctx.clone(),
            inst: &inst,
            io: None,
            tx: stub_tx,
            runtime_devs: Default::default(),
            dropped: Default::default(),
            transport: "mqtt",
            scripts_root: &scripts_root,
            role: core_::api::Role::North,
            self_writes: None, // North 无 EventBus 订阅面，无自环（T8）
            alarm: alarm.clone(),
        };

        // 3. VM 组装：沙箱 → register + cfg 表（先于 load_main——脚本顶层可用
        //    ctx/cfg）→ load_main → manifest dry-run（@role mqtt-north → auth+up
        //    必需；坏契约拒载 warn + return → supervisor 标 Failed）。
        let mut lua = match core_::engine::new_vm_sandboxed(cfg.hook_instr_limit, cfg.mem_limit_mb)
        {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("[luaNorth] inst {} VM 创建失败：{e}", inst.inst_id);
                return;
            }
        };
        let lctx = LoadCtx {
            scripts_root: std::sync::Arc::from(scripts_root.as_path()),
            script_name: std::sync::Arc::<str>::from(cfg.script.as_str()),
        };
        if let Err(e) = core_::api::register(&lua, &ac) {
            tracing::warn!("[luaNorth] inst {} ctx API 注册失败：{e}", inst.inst_id);
            return;
        }
        if let Err(e) = inject_cfg_table(&lua, &cfg, &ac) {
            tracing::warn!("[luaNorth] inst {} cfg 表注入失败：{e}", inst.inst_id);
            return;
        }
        if let Err(e) = load_main(&lua, &lctx) {
            tracing::warn!("[luaNorth] inst {} 脚本加载失败：{e}", inst.inst_id);
            return;
        }
        let stdlib_ver = core_::stdlib_sync::read_index(&scripts_root).map(|i| i.version);
        match core_::manifest::dry_run_main(
            &lua,
            &scripts_root,
            cfg.script.as_str(),
            stdlib_ver.as_deref(),
        ) {
            Ok(summary) => {
                // 复审 P3-4：成功路径 warn → info（成功消息不污染 warn 流）
                tracing::info!(
                    "[luaNorth] inst {} manifest dry-run 通过：{summary}",
                    inst.inst_id
                )
            }
            Err(e) => {
                tracing::warn!(
                    "[luaNorth] inst {} manifest dry-run 拒载：{e}",
                    inst.inst_id
                );
                return;
            }
        }

        // 4. 热加载 watcher + 批量/flush 双 ticker + stat 埋点（照 luaClient）。
        let (reload_tx, mut reload_rx) = mpsc::channel::<()>(8);
        let script_dir = scripts_root.join(cfg.script.as_str());
        core_::hotload::spawn_watcher(script_dir, reload_tx, token.clone());
        let batch_period = std::time::Duration::from_secs(cfg.batch_max_secs.max(1));
        let mut batch_ticker = tokio::time::interval(batch_period);
        batch_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // 5. mqtt task + 通道（spike-2 模型：网络 IO 独占，回调全在主循环侧）。
        let (mqtt_cmd_tx, mqtt_cmd_rx) = mpsc::channel::<contract::MqttCmd>(64);
        let (mqtt_evt_tx, mut mqtt_evt_rx) = mpsc::channel::<contract::MqttEvt>(64);
        // 句柄保留：主循环退出时 abort（pump 独占 poll 不进 select，token 观测
        // 不到其内部——复审阻塞-1 重构后 handler 虽有 token 分支，abort 作最终
        // 确定性清理，同 io_join.abort 惯例）。
        let mqtt_task = contract::spawn_mqtt_task(mqtt_cmd_rx, mqtt_evt_tx, token.clone());

        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        tracker.extra_insert(
            "script_version",
            serde_json::json!(core_::script::script_version(&scripts_root, &cfg.script)),
        );
        let mut vm_restarts: u64 = 0;
        let mut flush_ticker = tokio::time::interval(std::time::Duration::from_secs(
            crate::plugin::stat_tracker::FLUSH_INTERVAL_SECS,
        ));
        // T7 stat 计数（Q5）：发布成败与重连次数（flush 点落 extra）。
        let mut publish_ok: u64 = 0;
        let mut publish_fail: u64 = 0;
        let mut reconnect_count: u64 = 0;
        // down 丢弃计数（复审 P3 清偿）：down 失败/超预算跳过的下行条数——本地
        // 累计，flush 时经 sync_north_stats 落 extra（照 transform rtdata_skipped 模式）。
        let mut down_skipped: u64 = 0;

        // 初始连接（Q5 序列：will → auth → password 解析 → MqttOptions → Connect）。
        // Fatal（password secret ref 非法 / lux 错误 / 通道关闭）= 启动失败 warn 退出；
        // Retry（auth 回调失败等）→ 置退避定时器走重连循环（reload 可救）。
        let reconnect_backoff = std::time::Duration::from_secs(cfg.period.max(1));
        let mut reconnect_at = tokio::time::Instant::now();
        let mut reconnect_pending = false;
        let mut connected = false;
        match contract::start_connect(
            &lua,
            &cfg,
            &ctx.lux,
            ctx.hwid.as_str(),
            inst.inst_id.as_str(),
            &mqtt_cmd_tx,
        )
        .await
        {
            Ok(()) => {}
            Err(e) if e.fatal => {
                tracing::warn!(
                    "[luaNorth] inst {} 启动失败（{}）——配置级错误，退出实例",
                    inst.inst_id,
                    e.msg
                );
                tracker.set_status("stopped");
                tracker.flush().await;
                return;
            }
            Err(e) => {
                tracing::warn!(
                    "[luaNorth] inst {} 初始连接失败（{}），{}s 后重试",
                    inst.inst_id,
                    e.msg,
                    reconnect_backoff.as_secs()
                );
                reconnect_pending = true;
                reconnect_at = tokio::time::Instant::now() + reconnect_backoff;
            }
        }
        tracing::info!(
            "[luaNorth] inst {} started (script={}, broker={}:{}, batch ≤{}条/{}s)",
            inst.inst_id,
            cfg.script,
            cfg.host,
            cfg.port,
            cfg.batch_max_count,
            cfg.batch_max_secs
        );

        // 批缓冲（取舍 1：rx 样本与 lux pull 批量共用出口，flush 消费后清空）。
        let mut batch: Vec<Sample> = Vec::new();
        let mut batch_ticks: u64 = 0;
        // 满批/到时统一触发标志（match 内置位、match 后执行 flush）。
        let mut flush_now = false;

        loop {
            let evt = tokio::select! {
                _ = token.cancelled() => break,
                // 控制通道 "reload" → Evt::Reload（VM 级替换，Q11 同语义）
                msg = cmd_rx.recv() => match msg {
                    Some(m) if m.cmd == "reload" => Evt::Reload,
                    other => Evt::Cmd(other),
                },
                // Some 模式：rx 关闭（start_sink 的 tx 即弃——本阶段恒闲置）时该分支
                // 永久禁用，不忙循环也不退出（语义正确消费，C 阶段 tap 零改动）。
                Some(s) = rx.recv() => Evt::Sample(s),
                Some(()) = reload_rx.recv() => Evt::Reload,
                Some(ev) = mqtt_evt_rx.recv() => Evt::Mqtt(ev),
                // 重连退避到期（仅 pending 时启用；Instant 固定，每轮重建 future 无漂移）。
                _ = tokio::time::sleep_until(reconnect_at), if reconnect_pending => {
                    Evt::ReconnectNow
                }
                _ = flush_ticker.tick() => {
                    sync_north_stats(
                        &mut tracker,
                        publish_ok,
                        publish_fail,
                        reconnect_count,
                        down_skipped,
                        &alarm,
                    );
                    tracker.flush().await;
                    continue;
                }
                _ = batch_ticker.tick() => Evt::BatchTick,
            };
            match evt {
                Evt::Cmd(Some(m)) => {
                    // mqtt-north 回调契约无 on_write（§5.1）——write 等命令忽略 + debug。
                    tracing::debug!(
                        "[luaNorth] inst {} 忽略命令 {}（mqtt-north 契约无 on_write）",
                        inst.inst_id,
                        m.cmd
                    );
                }
                Evt::Cmd(None) => break,
                Evt::Sample(s) => {
                    // 入批缓冲 + 容量截断（丢最旧保最新；flush 清空）。
                    if batch.len() >= cfg.batch_max_count {
                        batch.remove(0);
                    }
                    batch.push(s);
                    // 满批即发（Q5 混合批量 count 维，复审 P1-1）：此前只有时间维，
                    // 突发下持续丢最旧直至下个 tick——丢弃窗口远超设计意图。
                    // 仅统计 rx 入批样本（lux pull 快照按全量语义不受 N 约束）。
                    if connected && batch.len() >= cfg.batch_max_count {
                        flush_now = true;
                    }
                }
                Evt::BatchTick => {
                    batch_ticks += 1;
                    // 断连期跳过 flush：数据留缓冲（容量截断保最新），重连后首批带上
                    // 积压——不白跑 up() 也不制造 publish_fail 计数噪音。
                    if connected {
                        flush_now = true;
                    } else {
                        tracing::debug!(
                            "[luaNorth] inst {} batch tick #{}（断连未 flush）",
                            inst.inst_id,
                            batch_ticks
                        );
                    }
                }
                Evt::Mqtt(ev) => match ev {
                    contract::MqttEvt::ConnAck => {
                        connected = true;
                        tracker.set_status("running");
                        // on_connack（可选回调；超预算 warn 不影响连接态）。
                        if let Err(e) = core_::budget::with_budget(
                            "on_connack",
                            cfg.callback_timeout_ms,
                            contract::call_on_connack(&lua),
                        )
                        .await
                        {
                            tracing::warn!("[luaNorth] inst {} on_connack 失败：{e}", inst.inst_id);
                        }
                        // 订阅 cfg.subscribe（Q5：+ 兼容场景脚本自定 topic；空列表不订阅）。
                        if !cfg.subscribe.is_empty() {
                            let topics = cfg.subscribe.clone();
                            if let Err(e) =
                                mqtt_cmd_tx.send(contract::MqttCmd::Subscribe(topics)).await
                            {
                                tracing::warn!(
                                    "[luaNorth] inst {} subscribe 下发失败：{e}",
                                    inst.inst_id
                                );
                            }
                        }
                    }
                    contract::MqttEvt::Disconnected(reason) => {
                        connected = false;
                        reconnect_count += 1;
                        tracker.set_status("degraded");
                        tracing::warn!(
                            "[luaNorth] inst {} MQTT 断连（{reason}），{}s 后重连（重调 auth）",
                            inst.inst_id,
                            reconnect_backoff.as_secs()
                        );
                        // on_disconnect（可选回调；超预算 warn 不影响重连节奏）。
                        if let Err(e) = core_::budget::with_budget(
                            "on_disconnect",
                            cfg.callback_timeout_ms,
                            contract::call_on_disconnect(&lua),
                        )
                        .await
                        {
                            tracing::warn!(
                                "[luaNorth] inst {} on_disconnect 失败：{e}",
                                inst.inst_id
                            );
                        }
                        reconnect_pending = true;
                        reconnect_at = tokio::time::Instant::now() + reconnect_backoff;
                    }
                    contract::MqttEvt::Down { topic, payload } => {
                        // down(topic, payload)（可选回调；T7 全量路由——命令 topic 是
                        // T8 兼容脚本概念）。脚本错误与超预算同口径：warn + tick_fail
                        //（degraded 判定，同 lua_client 事件驱动回调语义）+ down_skipped
                        // 计数（丢弃量可见）。此前 `if let Err` 只捕 with_budget 的
                        // 超预算层，脚本错误以 Ok(Err) 形态被静默吞——本批补双层处理。
                        match core_::budget::with_budget(
                            "down",
                            cfg.callback_timeout_ms,
                            contract::call_down(&lua, &topic, &payload),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) | Err(e) => {
                                down_skipped += 1;
                                tracing::warn!(
                                    "[luaNorth] inst {} down 失败（topic={topic}）：{e}",
                                    inst.inst_id
                                );
                                tracker.tick_fail(&e.to_string());
                            }
                        }
                    }
                    contract::MqttEvt::PubResult { ok, fail } => {
                        publish_ok += ok as u64;
                        publish_fail += fail as u64;
                    }
                },
                Evt::ReconnectNow => {
                    // Q5 核心：重连完整重走连接序列（重调 auth——token 型认证刷新）。
                    reconnect_pending = false;
                    match contract::start_connect(
                        &lua,
                        &cfg,
                        &ctx.lux,
                        ctx.hwid.as_str(),
                        inst.inst_id.as_str(),
                        &mqtt_cmd_tx,
                    )
                    .await
                    {
                        Ok(()) => {}
                        Err(e) if e.fatal => {
                            tracing::warn!(
                                "[luaNorth] inst {} 重连序列 Fatal（{}），退出实例",
                                inst.inst_id,
                                e.msg
                            );
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(
                                "[luaNorth] inst {} 重连失败（{}），{}s 后再试",
                                inst.inst_id,
                                e.msg,
                                reconnect_backoff.as_secs()
                            );
                            reconnect_pending = true;
                            reconnect_at = tokio::time::Instant::now() + reconnect_backoff;
                        }
                    }
                }
                Evt::Reload => match reload_vm(&cfg, &lctx, &ac) {
                    Ok(new_lua) => {
                        // 新 VM 就位（旧 VM 随替换释放）；契约回调（auth/up/down…）
                        // 从新 VM 调用。
                        lua = new_lua;
                        vm_restarts += 1;
                        tracker.extra_insert("vm_restart_count", serde_json::json!(vm_restarts));
                        tracker.extra_insert(
                            "script_version",
                            serde_json::json!(core_::script::script_version(
                                &scripts_root,
                                &cfg.script
                            )),
                        );
                        tracker.set_status(if connected { "running" } else { "degraded" });
                        tracker.flush().await;
                        tracing::info!("[luaNorth] inst {} 热加载成功", inst.inst_id);
                    }
                    Err(e) => tracing::warn!(
                        "[luaNorth] inst {} 热加载失败，旧 VM 继续：{e}",
                        inst.inst_id
                    ),
                },
            }
            // flush 统一后处理（满批即发 / batch tick 同路径；复审 P1-1 重构）。
            // 失败时样本由 flush_batch 内部回填缓冲（复审 P2-3），本轮不弃数据。
            if flush_now {
                flush_now = false;
                let started = std::time::Instant::now();
                match contract::flush_batch(&lua, &ctx.lux, &cfg, &mut batch, &mqtt_cmd_tx).await {
                    Ok(contract::FlushOutcome::Skipped) => {}
                    Ok(contract::FlushOutcome::Up { parse_fail }) => {
                        if parse_fail > 0 {
                            publish_fail += parse_fail as u64;
                        }
                        tracker.tick_ok(started.elapsed().as_millis() as u64);
                    }
                    Err(e) => {
                        // up 超预算/失败：跳过本轮 + tick_fail（degraded 判定）；
                        // 样本已回填缓冲（flush_batch P2-3），下轮重试。
                        tracing::warn!(
                            "[luaNorth] inst {} flush 失败（{}），本轮数据留缓冲",
                            inst.inst_id,
                            e
                        );
                        tracker.tick_fail(&e);
                    }
                }
                if tracker.should_flush() {
                    sync_north_stats(
                        &mut tracker,
                        publish_ok,
                        publish_fail,
                        reconnect_count,
                        down_skipped,
                        &alarm,
                    );
                    tracker.flush().await;
                }
            }
        }

        // mqtt task 确定性终止（token 之外的双保险）
        mqtt_task.abort();
        // 退出标记 stopped（spec §13；实例生命周期经由 InstanceManager 全套）。
        tracker.set_status("stopped");
        sync_north_stats(
            &mut tracker,
            publish_ok,
            publish_fail,
            reconnect_count,
            down_skipped,
            &alarm,
        );
        tracker.flush().await;
    }
}

/// T7 stat 计数快照（Q5）：publish_ok/publish_fail/reconnect_count 落 tracker
/// extra（每个 flush 点前调用，不留陈旧）。告警频控计数同出口（Q12 T4）；
/// down_skipped 同出口（复审 P3 清偿：down 丢弃量可见，照 transform
/// rtdata_skipped 模式——本地累计 u64 快照落 extra）。
fn sync_north_stats(
    tracker: &mut crate::plugin::stat_tracker::StatTracker,
    publish_ok: u64,
    publish_fail: u64,
    reconnect_count: u64,
    down_skipped: u64,
    alarm: &core_::api::AlarmStats,
) {
    tracker.extra_insert("publish_ok", serde_json::json!(publish_ok));
    tracker.extra_insert("publish_fail", serde_json::json!(publish_fail));
    tracker.extra_insert("reconnect_count", serde_json::json!(reconnect_count));
    tracker.extra_insert("down_skipped", serde_json::json!(down_skipped));
    // Q12 T4（spec alarm §3）：fire_event 被频控拒绝累计（风暴可见性）。
    tracker.extra_insert(
        "alarm_rate_limited",
        serde_json::json!(alarm.rate_limited_count()),
    );
    // 订阅失败累计（复审 P2-5）：下行通道静默失效可见化（进程级计数，多实例共用）
    tracker.extra_insert(
        "subscribe_fail",
        serde_json::json!(contract::SUBSCRIBE_FAIL.load(std::sync::atomic::Ordering::Relaxed)),
    );
}

// 静态注册到 inventory（编译期，编进单一二进制）——SinkEntry 首个接线者。
inventory::submit! {
    crate::plugin::registry::SinkEntry {
        name: "luaNorth",
        create: || Box::new(LuaNorth),
    }
}
