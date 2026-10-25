//! 插件实例生命周期管理。
//!
//! 对标 Lite_IOT `sysutil.StartInst/StopInst` + `InstStatusMap`。
//!
//! 数据流拓扑（M3 hub 模式）：所有 Source 的输出汇聚到一个共享 mpsc `hub_tx`，
//! pipeline 单消费者从 `hub_rx` 拉取。这样 REST 动态启停某个 Source 不会断
//! 管道（其他 Source 与 pipeline、Sink 继续工作）。

use crate::error::RsIotError;
use crate::host::HostContext;
use crate::plugin::registry;
use crate::plugin::sample_sender::SampleSender;
use crate::types::{CtlMessage, InstConfig, Sample};
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// 插件实例运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstState {
    /// 正在启动。
    Starting,
    /// 运行中。
    Running,
    /// 正在停止。
    Stopping,
    /// 已停止。
    Stopped,
    /// 失败。
    Failed,
}

#[derive(Debug)]
struct InstanceHandle {
    token: CancellationToken,
    /// supervisor task 的 handle（M3 自愈可信）：supervisor 内部唯一 await source/sink
    /// task 的 join，按 token 判定 Failed。`stop` await 此 handle，3s 超时语义不变。
    join: JoinHandle<()>,
}

/// 实例管理器：运行态表 + task handle 表 + 数据汇聚 hub。
///
/// 含 `HostContext`（其 `EmbeddedClient` 无 `Debug`），故以 `expect` 放行 `missing_debug_implementations`
///（若未来 EmbeddedClient 补 Debug，编译器会提示移除该 expect）。
#[expect(missing_debug_implementations)]
pub struct InstanceManager {
    ctx: HostContext,
    states: Arc<DashMap<String, InstState>>,
    handles: Arc<DashMap<String, InstanceHandle>>,
    /// 实例配置快照（start 时记，stop 保留；/inst/stats 分类数据源）。
    configs: Arc<DashMap<String, InstConfig>>,
    /// 所有 Source 输出汇聚点（pipeline 消费 `hub_rx`）。
    hub_tx: mpsc::Sender<Sample>,
}

/// 唯一启动守卫的原子认领（start_source/start_sink 共用）：states 无条目或终态
/// （Stopped/Failed）→ 置 Starting 放行；活跃态（Starting/Running/Stopping）→
/// [`RsIotError::InstanceAlreadyRunning`]。`entry` 的检查-写入在 shard 锁内一次
/// 完成，并发二次启动只有一个能认领成功（且本函数不含 await，锁不跨挂起点）。
fn claim_starting(states: &DashMap<String, InstState>, id: &str) -> Result<(), RsIotError> {
    use dashmap::mapref::entry::Entry;
    match states.entry(id.to_string()) {
        Entry::Occupied(mut e) => match e.get() {
            InstState::Starting | InstState::Running | InstState::Stopping => {
                Err(RsIotError::InstanceAlreadyRunning(id.to_string()))
            }
            InstState::Stopped | InstState::Failed => {
                e.insert(InstState::Starting);
                Ok(())
            }
        },
        Entry::Vacant(e) => {
            e.insert(InstState::Starting);
            Ok(())
        }
    }
}

impl InstanceManager {
    /// 构造管理器，绑定到共享 hub 发送端。
    pub fn new(ctx: HostContext, hub_tx: mpsc::Sender<Sample>) -> Self {
        Self {
            ctx,
            states: Arc::new(DashMap::new()),
            handles: Arc::new(DashMap::new()),
            configs: Arc::new(DashMap::new()),
            hub_tx,
        }
    }

    /// 快照所有实例当前状态（供 `/api/v1/inststatus`）。
    pub fn status(&self) -> HashMap<String, InstState> {
        self.states
            .iter()
            .map(|r| (r.key().clone(), *r.value()))
            .collect()
    }

    /// 启动南向 Source 实例。产出汇入共享 hub（非返回 rx）。
    #[tracing::instrument(skip(self, inst), fields(hwid = %self.ctx.hwid, inst_id = %inst.inst_id))]
    pub async fn start_source(&self, inst: InstConfig) -> Result<(), RsIotError> {
        let create = registry::find_source(inst.app_code.as_ref()).ok_or_else(|| {
            RsIotError::Config(format!("source plugin not found: {}", inst.app_code))
        })?;
        let source = create();
        let id_key = inst.inst_id.to_string();

        let (cmd_tx, cmd_rx) = mpsc::channel::<CtlMessage>(64);
        let token = self.ctx.shutdown.child_token();

        // 唯一启动守卫（原子认领）：同 inst_id 活跃态（Starting/Running/Stopping）
        // → 拒绝。此前裸 insert 顶替注册表条目，旧任务无人 cancel 成"幽灵任务"
        // （双份采样/CPU 且 instance_info 单行不可见）。Stopped/Failed 放行——
        // stop 后可重启（ed76dc2 语义）。entry 持锁作用域不跨 await。
        claim_starting(&self.states, &id_key)?;
        self.configs.insert(id_key.clone(), inst.clone());
        self.ctx.ctl_registry.register(&id_key, cmd_tx);
        self.emit_state(&id_key, InstState::Starting);

        let ctx = self.ctx.clone();
        // D-OWN-2 启动预登记：绑定 devices[].devid 在首条数据前就入册 dev2inst——
        // 防"启动后未出数即删"漏归属（SampleSender 只登记实际出过数的 devid，
        // 预登记补齐零出数窗口）。失败仅 warn：登记是管理面记账，最终一致；
        // cascade 的配置回退路径仍是安全网（见 routes_inst::cascade_delete）。
        for dev in &inst.devices {
            if dev.devid.is_empty() {
                continue;
            }
            if let Err(e) = crate::db::dev_registry::note(&self.ctx.lux, &dev.devid, &id_key).await
            {
                tracing::warn!(
                    "[instance] dev2inst 预登记 {} <- {id_key} 失败（不阻断启动）: {e}",
                    dev.devid
                );
            }
        }
        // D-OWN-1 唯一包装点：Source 出口经 SampleSender 遮蔽 send/try_send 登记
        // dev2inst（归属 = 本实例）；seen 去重防每 tick 旁路写，详见 sample_sender。
        let tx = SampleSender::new(self.hub_tx.clone(), self.ctx.lux.clone(), id_key.clone());
        let token_run = token.clone();
        let id_for_log = id_key.clone();
        let source_join = tokio::spawn(async move {
            source.run(ctx, inst, tx, cmd_rx, token_run).await;
            tracing::info!("[instance] source {id_for_log} exited");
        });

        let sup_join = self.spawn_supervisor(id_key.clone(), token.clone(), source_join);

        self.states.insert(id_key.clone(), InstState::Running);
        self.emit_state(&id_key, InstState::Running);
        self.handles.insert(
            id_key,
            InstanceHandle {
                token,
                join: sup_join,
            },
        );
        Ok(())
    }

    /// 启动北向 Sink 实例。
    ///
    /// 数据面 B 阶段走 lux pull（luaNorth 批量收拢），sample 通道由内部自建：
    /// tx 即 drop、闲置 rx 传入 run——pipeline 未来 tap 的预留位（阶段 B 计划
    /// 取舍 1；tap 落地时再加 `start_sink_tapped` 变体）。
    #[tracing::instrument(skip(self, inst), fields(hwid = %self.ctx.hwid, inst_id = %inst.inst_id))]
    pub async fn start_sink(&self, inst: InstConfig) -> Result<(), RsIotError> {
        let create = registry::find_sink(inst.app_code.as_ref()).ok_or_else(|| {
            RsIotError::Config(format!("sink plugin not found: {}", inst.app_code))
        })?;
        let sink = create();
        let id_key = inst.inst_id.to_string();

        let (cmd_tx, cmd_rx) = mpsc::channel::<CtlMessage>(64);
        let (_sample_tx, sample_rx) = mpsc::channel::<Sample>(64);
        let token = self.ctx.shutdown.child_token();

        // 唯一启动守卫（同 start_source，见其注释）。
        claim_starting(&self.states, &id_key)?;
        self.configs.insert(id_key.clone(), inst.clone());
        self.ctx.ctl_registry.register(&id_key, cmd_tx);
        self.emit_state(&id_key, InstState::Starting);

        let ctx = self.ctx.clone();
        let token_run = token.clone();
        let id_for_log = id_key.clone();
        let sink_join = tokio::spawn(async move {
            sink.run(ctx, inst, sample_rx, cmd_rx, token_run).await;
            tracing::info!("[instance] sink {id_for_log} exited");
        });

        let sup_join = self.spawn_supervisor(id_key.clone(), token.clone(), sink_join);

        self.states.insert(id_key.clone(), InstState::Running);
        self.emit_state(&id_key, InstState::Running);
        self.handles.insert(
            id_key,
            InstanceHandle {
                token,
                join: sup_join,
            },
        );
        Ok(())
    }

    /// 停止指定实例：cancel 其 token 并等 join（3s 超时）。
    #[tracing::instrument(skip(self), fields(hwid = %self.ctx.hwid, inst_id = %inst_id))]
    pub async fn stop(&self, inst_id: &str) -> Result<(), RsIotError> {
        // 防御：实例从未注册（未启动或已删除）时 stop 是无操作——**不**往 states 插入
        // Stopping/Stopped。否则对已删除实例调 stop 会留下永久状态条目，/inst/stats
        // 持续显示已删除实例（删除级联的次生 bug：cascade_delete 对未启动实例也调 stop）。
        if !self.handles.contains_key(inst_id) && !self.states.contains_key(inst_id) {
            return Ok(());
        }
        self.states.insert(inst_id.to_string(), InstState::Stopping);
        self.emit_state(inst_id, InstState::Stopping);
        self.ctx.ctl_registry.unregister(inst_id);
        if let Some((_, h)) = self.handles.remove(inst_id) {
            h.token.cancel();
            match tokio::time::timeout(Duration::from_secs(3), h.join).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("[instance] {inst_id} join error: {e}"),
                Err(_) => tracing::warn!("[instance] {inst_id} stop timeout (3s)"),
            }
        }
        self.states.insert(inst_id.to_string(), InstState::Stopped);
        self.emit_state(inst_id, InstState::Stopped);
        Ok(())
    }

    /// 彻底移除实例运行态记录（**删除**场景专用，区别于 `stop`）。
    ///
    /// `stop` 仅将 `states` 置 `Stopped`（保留死活历史，供 /inst/stats 显示"刚停止"），
    /// 但删除实例应不留残影——需额外清 `states` + `configs`（`handles` 已由 stop 移除）。
    /// 否则 /inst/stats 持续返回已删除实例（实测残留 bug：页面 Ctrl+F5 后仍见已删实例 stopped）。
    pub fn remove_state(&self, inst_id: &str) {
        self.states.remove(inst_id);
        self.configs.remove(inst_id);
        // handles 已由 stop 移除；防御性再清一次（未走 stop 直接 remove 的理论场景）。
        self.handles.remove(inst_id);
    }

    /// 定向控制消息直达（无回执）：经 `CtlRegistry` 投递到实例 task 的 cmd_rx。
    ///
    /// 实例未运行（`stop` 已 unregister / 从未启动）返回 false——批量 reload 等
    /// 隐式全量操作据此跳过非运行实例，不复活管理员已停的实例（Q11 语义，
    /// 阶段 A 审查修复）。
    pub fn ctl_send(&self, inst_id: &str, cmd: &str, data: &str) -> bool {
        self.ctx.ctl_registry.send(
            inst_id,
            CtlMessage {
                instance: crate::types::InstId::new(inst_id),
                devid: String::new(),
                cmd: cmd.to_string(),
                data: data.to_string(),
            },
        )
    }

    /// 停止所有实例（关闭流程用）。
    pub async fn stop_all(&self) {
        let ids: Vec<String> = self.handles.iter().map(|r| r.key().clone()).collect();
        for id in ids {
            let _ = self.stop(&id).await;
        }
    }

    /// spawn supervisor（M3 自愈可信）：唯一 await inner task join。
    /// task 退出（panic 的 JoinError 单独 warn 便于诊断）且 token 未 cancel → 标 Failed + 告警。
    /// token 已 cancel = 主动 stop（stop 已标 Stopping→Stopped），不干预。
    fn spawn_supervisor(
        &self,
        id: String,
        token: CancellationToken,
        inner: JoinHandle<()>,
    ) -> JoinHandle<()> {
        let states = self.states.clone();
        let event_tx = self.ctx.event_tx.clone();
        let configs = self.configs.clone();
        tokio::spawn(async move {
            if let Err(e) = inner.await {
                tracing::warn!("[instance] {id} task join error: {e}");
            }
            if !token.is_cancelled() {
                states.insert(id.clone(), InstState::Failed);
                tracing::error!("[instance] {id} task exited unexpectedly → Failed");
                // SSE 推送 Failed 状态：前端概览即时变（不等 2s 轮询）。
                // configs 闭包内查（无 self）；缺配置（极端：start 中途 configs 被清）静默跳过。
                if let Some(cfg) = configs.get(&id).map(|r| r.value().clone()) {
                    event_tx.publish(crate::event_bus::EventPayload::State {
                        inst_id: id.clone(),
                        state: InstState::Failed,
                        app_code: cfg.app_code.as_ref().to_string(),
                        host_kind: cfg.host_kind,
                        ts: crate::event_bus::now_iso8601(),
                    });
                }
            }
        })
    }

    /// 投递控制命令到运行中的实例（经 `CtlRegistry` 路由到实例 `cmd_rx`）。
    ///
    /// 供 REST/MQTT/未来北向服务器等写入口统一调用。返回 `false` = 实例未注册（未启动）；
    /// channel 满仍 `try_send` 丢弃（对标 Go 版 `default` 非阻塞），此时返回 `true`。
    pub fn send_ctl(&self, inst: &str, msg: CtlMessage) -> bool {
        self.ctx.ctl_registry.send(inst, msg)
    }

    /// 取实例配置快照（/inst/stats 分类用：app_code + host_kind）。
    pub fn config_snapshot(&self, inst_id: &str) -> Option<InstConfig> {
        self.configs.get(inst_id).map(|r| r.value().clone())
    }

    /// SSE 推送实例状态变更（阶段一）：start_source/start_sink/stop 的状态流转 + supervisor
    /// 的 Failed 分支统一经此发布 `EventPayload::State`。前端概览状态计数即时刷新（不等 2s 轮询）。
    ///
    /// 用 `config_snapshot` 取 app_code + host_kind（缺配置静默跳过——如实例已被清）。
    /// 无 SSE 消费者时 `publish` 静默零开销。
    fn emit_state(&self, inst_id: &str, state: InstState) {
        let Some(cfg) = self.config_snapshot(inst_id) else {
            return;
        };
        self.ctx
            .event_tx
            .publish(crate::event_bus::EventPayload::State {
                inst_id: inst_id.to_string(),
                state,
                app_code: cfg.app_code.as_ref().to_string(),
                host_kind: cfg.host_kind,
                ts: crate::event_bus::now_iso8601(),
            });
    }
}
