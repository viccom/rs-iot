//! 宿主上下文与命令路由注册表。

use crate::types::CtlMessage;
use dashmap::DashMap;
use lux::EmbeddedClient;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 命令路由注册表：`instId` → 命令 channel。
///
/// 对标 Lite_IOT `pluginM.handlers map[string]chan CtlMessage` +
/// `SendMessage(target, msg)`。北向（MQTT command）或 REST 把命令投到这里，
/// 再路由到对应实例 task 的 `cmd_rx`。
#[derive(Clone, Default, Debug)]
pub struct CtlRegistry {
    map: Arc<DashMap<String, mpsc::Sender<CtlMessage>>>,
}

impl CtlRegistry {
    /// 构造空注册表。
    pub fn new() -> Self {
        Self {
            map: Arc::new(DashMap::new()),
        }
    }

    /// 注册实例的命令接收端。
    pub fn register(&self, inst: &str, tx: mpsc::Sender<CtlMessage>) {
        self.map.insert(inst.to_string(), tx);
    }

    /// 注销（实例停止时）。
    pub fn unregister(&self, inst: &str) {
        self.map.remove(inst);
    }

    /// 投递命令到指定实例。channel 满则丢弃（对标 Go 版 `default` 非阻塞语义）。
    pub fn send(&self, inst: &str, msg: CtlMessage) {
        // clone Sender 后立即释放 DashMap 读锁，避免跨 await 持锁。
        let tx = match self.map.get(inst) {
            Some(ref_tx) => ref_tx.value().clone(),
            None => return,
        };
        drop(tx);
        // 重新取引用并 try_send（不持锁跨 await）
        if let Some(ref_tx) = self.map.get(inst) {
            let _ = ref_tx.value().try_send(msg);
        }
    }

    /// 是否已注册某实例。
    pub fn contains(&self, inst: &str) -> bool {
        self.map.contains_key(inst)
    }
}

/// 注入给每个插件 task 的宿主能力。
///
/// `lux::EmbeddedClient` 未实现 `Debug`，故本类型暂以 `allow` 放行 `missing_debug_implementations`。
#[allow(missing_debug_implementations)]
#[derive(Clone)]
pub struct HostContext {
    /// 进程内 lux 客户端（零拷贝，Clone 共享 store/WAL）。
    pub lux: EmbeddedClient,
    /// 边缘端硬件 ID。
    pub hwid: String,
    /// 全局关闭信号。
    pub shutdown: CancellationToken,
    /// 命令路由表。
    pub ctl_registry: CtlRegistry,
}
