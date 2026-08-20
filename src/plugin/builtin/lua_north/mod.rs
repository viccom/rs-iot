//! `luaNorth`：mqtt-north 角色 Lua 薄壳（**Sink trait 首个接线者**，Q19/R4）。
//!
//! 阶段 B T6 骨架：实例生命周期（InstanceManager 全套）+ VM 组装（沙箱/
//! loader/能力包裁剪）+ cfg 全局表（Q5）+ manifest 多角色契约（`@role
//! mqtt-north` → auth+up 必需）+ 热加载 + stat 埋点。**MQTT 连接与契约回调
//! （auth/up/down/will/on_connack/on_disconnect + lux pull 收拢批量）是 T7**，
//! 本文件主循环只立骨架（批缓冲入队 + batch tick 计数 + TODO 标注）。
//!
//! 与 luaClient 的结构差异（§5.0「一套沙箱，多种角色」）：
//! - register 先于 load_main（collector 相反）——mqtt-north 脚本顶层即可用
//!   `ctx`/`cfg`（能力探针/配置读取），契约回调 T7 才被调用；
//! - ApiCtx 角色 North：C1（request/transport）不注册，`io: None` 不建 IO 引擎
//!   （§5.2 矩阵 mqtt-north 列）；
//! - 数据收拢走 lux pull 快照批量（取舍 1，Q6）：Sink `rx` 恒闲置但语义正确
//!   消费——收到即入批缓冲，C 阶段 pipeline tap 落地零改动。

mod config;

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
    /// 样本（rx；取舍 1：入批缓冲，T7 flush 消费）。
    Sample(Sample),
    /// 批量周期（batch_max_secs；T7 的 lux pull 收拢 + up() 发布点）。
    BatchTick,
    /// 热加载信号（脚本目录 watcher 或 cmd_rx "reload"；VM 级替换，失败回退）。
    Reload,
}

/// mqtt-north 北向薄壳（Sink）。
#[derive(Debug, Default)]
pub struct LuaNorth;

/// 注入全局 `cfg` 表（Q5「宿主另注入全局 cfg 表」/ T7 `auth(cfg)` 参数）：
/// NorthCfg 有效子集（client_id 缺省已物化；password 保留 `secret:` 引用原样——
/// 解析收敛在 T7 连接前一刻，明文不长期驻留 VM globals）。
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
/// 契约回调（on_init 等）的 dry-run 是 T7——本阶段回调尚未接线，加载期指令
/// hook 已兜底纯计算死循环（§9）。
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
        // 1. NorthCfg（broker/批量节奏；无传输项——MQTT 连接层在宿主，T7）。
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

        // 2. ApiCtx（North 角色）：io=None（无传输层，C1 随角色裁剪不注册）；
        //    tx 为占位通道（接收端即弃）——C2 set_value(s) 在 north 语义下无
        //    上行出口（Sink 自身是数据消费者），T7 定案出口前推入即 Closed 告警。
        let (stub_tx, stub_rx) = mpsc::channel::<Sample>(1);
        drop(stub_rx);
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
                tracing::warn!(
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
        tracing::info!(
            "[luaNorth] inst {} started (script={}, broker={}:{}, batch ≤{}条/{}s, MQTT 连接 T7 接线)",
            inst.inst_id,
            cfg.script,
            cfg.host,
            cfg.port,
            cfg.batch_max_count,
            cfg.batch_max_secs
        );
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

        // 批缓冲（取舍 1：rx 样本 + T7 lux pull 批量共用出口）。
        let mut batch: Vec<Sample> = Vec::new();
        let mut batch_ticks: u64 = 0;

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
                _ = flush_ticker.tick() => { tracker.flush().await; continue; }
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
                    // 入批缓冲 + 容量截断（丢最旧保最新；flush 清空是 T7）。
                    if batch.len() >= cfg.batch_max_count {
                        batch.remove(0);
                    }
                    batch.push(s);
                }
                Evt::BatchTick => {
                    batch_ticks += 1;
                    // TODO(T7)：lux pull 收拢 rtdata → 组 batch（batch_max_count 分片）
                    //   → with_budget 调 up(batch) → 宿主逐条投递发布 → 清批缓冲；
                    //   失败 tick_fail 记账（degraded 判定）。
                    tracing::debug!(
                        "[luaNorth] inst {} batch tick #{}（up() 发布 T7 接线）",
                        inst.inst_id,
                        batch_ticks
                    );
                }
                Evt::Reload => match reload_vm(&cfg, &lctx, &ac) {
                    Ok(new_lua) => {
                        // 新 VM 就位（旧 VM 随替换释放）；T7 契约回调（auth/up/down…）
                        // 将从新 VM 调用——骨架期无读者，语句级 allow 压 unused_assignments。
                        #[allow(unused_assignments)]
                        {
                            lua = new_lua;
                        }
                        vm_restarts += 1;
                        tracker.extra_insert("vm_restart_count", serde_json::json!(vm_restarts));
                        tracker.extra_insert(
                            "script_version",
                            serde_json::json!(core_::script::script_version(
                                &scripts_root,
                                &cfg.script
                            )),
                        );
                        tracker.set_status("running");
                        tracker.flush().await;
                        tracing::info!("[luaNorth] inst {} 热加载成功", inst.inst_id);
                    }
                    Err(e) => tracing::warn!(
                        "[luaNorth] inst {} 热加载失败，旧 VM 继续：{e}",
                        inst.inst_id
                    ),
                },
            }
        }

        // 退出标记 stopped（spec §13；实例生命周期经由 InstanceManager 全套）。
        tracker.set_status("stopped");
        tracker.flush().await;
    }
}

// 静态注册到 inventory（编译期，编进单一二进制）——SinkEntry 首个接线者。
inventory::submit! {
    crate::plugin::registry::SinkEntry {
        name: "luaNorth",
        create: || Box::new(LuaNorth),
    }
}
