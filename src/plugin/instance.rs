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
    /// 所有 Source 输出汇聚点（pipeline 消费 `hub_rx`）。
    hub_tx: mpsc::Sender<Sample>,
}

impl InstanceManager {
    /// 构造管理器，绑定到共享 hub 发送端。
    pub fn new(ctx: HostContext, hub_tx: mpsc::Sender<Sample>) -> Self {
        Self {
            ctx,
            states: Arc::new(DashMap::new()),
            handles: Arc::new(DashMap::new()),
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

        self.states.insert(id_key.clone(), InstState::Starting);
        self.ctx.ctl_registry.register(&id_key, cmd_tx);

        let ctx = self.ctx.clone();
        let tx = self.hub_tx.clone();
        let token_run = token.clone();
        let id_for_log = id_key.clone();
        let join = tokio::spawn(async move {
            source.run(ctx, inst, tx, cmd_rx, token_run).await;
            tracing::info!("[instance] source {id_for_log} exited");
        });

        self.states.insert(id_key.clone(), InstState::Running);
        self.handles.insert(id_key, InstanceHandle { token, join });
        Ok(())
    }

    /// 启动北向 Sink 实例（sample_rx 由调用方/pipeline 提供）。
    #[tracing::instrument(skip(self, inst), fields(hwid = %self.ctx.hwid, inst_id = %inst.inst_id))]
    pub async fn start_sink(
        &self,
        inst: InstConfig,
        sample_rx: mpsc::Receiver<Sample>,
    ) -> Result<(), RsIotError> {
        let create = registry::find_sink(inst.app_code.as_ref()).ok_or_else(|| {
            RsIotError::Config(format!("sink plugin not found: {}", inst.app_code))
        })?;
        let sink = create();
        let id_key = inst.inst_id.to_string();

        let (cmd_tx, cmd_rx) = mpsc::channel::<CtlMessage>(64);
        let token = self.ctx.shutdown.child_token();

        self.states.insert(id_key.clone(), InstState::Starting);
        self.ctx.ctl_registry.register(&id_key, cmd_tx);

        let ctx = self.ctx.clone();
        let token_run = token.clone();
        let id_for_log = id_key.clone();
        let join = tokio::spawn(async move {
            sink.run(ctx, inst, sample_rx, cmd_rx, token_run).await;
            tracing::info!("[instance] sink {id_for_log} exited");
        });

        self.states.insert(id_key.clone(), InstState::Running);
        self.handles.insert(id_key, InstanceHandle { token, join });
        Ok(())
    }

    /// 停止指定实例：cancel 其 token 并等 join（3s 超时）。
    #[tracing::instrument(skip(self), fields(hwid = %self.ctx.hwid, inst_id = %inst_id))]
    pub async fn stop(&self, inst_id: &str) -> Result<(), RsIotError> {
        self.states.insert(inst_id.to_string(), InstState::Stopping);
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
        Ok(())
    }

    /// 停止所有实例（关闭流程用）。
    pub async fn stop_all(&self) {
        let ids: Vec<String> = self.handles.iter().map(|r| r.key().clone()).collect();
        for id in ids {
            let _ = self.stop(&id).await;
        }
    }

    /// 投递控制命令到运行中的实例（经 `CtlRegistry` 路由到实例 `cmd_rx`）。
    ///
    /// 供 REST/MQTT/未来北向服务器等写入口统一调用。返回 `false` = 实例未注册（未启动）；
    /// channel 满仍 `try_send` 丢弃（对标 Go 版 `default` 非阻塞），此时返回 `true`。
    pub fn send_ctl(&self, inst: &str, msg: CtlMessage) -> bool {
        self.ctx.ctl_registry.send(inst, msg)
    }
}
