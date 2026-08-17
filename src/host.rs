//! 宿主上下文与命令路由注册表。

use crate::types::CtlMessage;
use dashmap::DashMap;
use lux::EmbeddedClient;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 命令路由注册表：`instId` → 命令 channel。
///
/// 对标 Lite_IOT `pluginM.handlers map[string]chan CtlMessage` +
/// `SendMessage(target, msg)`。北向（MQTT command）或 REST 把命令投到这里，
/// 再路由到对应实例 task 的 `cmd_rx`。
#[derive(Clone, Debug)]
pub struct CtlRegistry {
    map: Arc<DashMap<String, mpsc::Sender<CtlMessage>>>,
    /// 进程级指标（M4）：send/dropped/not_found 分类计数。
    metrics: Arc<crate::process_metrics::ProcessMetrics>,
}

impl CtlRegistry {
    /// 构造，绑定进程级指标计数器（M4）。
    pub fn new(metrics: Arc<crate::process_metrics::ProcessMetrics>) -> Self {
        Self {
            map: Arc::new(DashMap::new()),
            metrics,
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

    /// 投递命令到指定实例。返回是否找到实例并尝试投递（channel 满仍 `try_send` 丢弃，
    /// 对标 Go 版 `default` 非阻塞；返回 `false` = 实例未注册，供写入口给调用方 404 反馈）。
    ///
    /// M4：分类计数（send/dropped/not_found）+ channel 满 warn（不再静默丢命令）。
    /// 返回值语义**不变**：命中 map → `true`（Full/Closed 也 `true`），未注册 → `false`。
    pub fn send(&self, inst: &str, msg: CtlMessage) -> bool {
        self.metrics.ctl_send();
        if let Some(ref_tx) = self.map.get(inst) {
            match ref_tx.value().try_send(msg) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.metrics.ctl_dropped();
                    tracing::warn!("[ctl] {inst} channel full, command dropped");
                    true
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.metrics.ctl_dropped();
                    true
                }
            }
        } else {
            self.metrics.ctl_not_found();
            false
        }
    }

    /// 是否已注册某实例。
    pub fn contains(&self, inst: &str) -> bool {
        self.map.contains_key(inst)
    }
}

/// bridge 连接分发失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchErr {
    /// 实例未启动（无 slot）。
    NoSlot,
    /// 实例 task 还没取走上一次连接（slot 满）。
    Full,
}

/// bridge 连接分发注册表：`inst_id` → 连接 slot。
///
/// acceptor 校验外部进程 Hello 后，按 `inst_id` 把裸 `TcpStream` 经此表分发给对应
/// sidecarClient task（每外部实例一个 task、一个 slot）。slot 用 mpsc 支持外部多次重连。
/// task 收到 stream 后自行 `tokio::io::split` + `FramedRead`/`FramedWrite`（codec 在 sidecar_client）。
#[derive(Clone, Default, Debug)]
pub struct BridgeRegistry {
    map: Arc<DashMap<String, mpsc::Sender<TcpStream>>>,
}

impl BridgeRegistry {
    /// 构造空注册表。
    pub fn new() -> Self {
        Self {
            map: Arc::new(DashMap::new()),
        }
    }

    /// 注册实例的连接 slot（sidecarClient task 启动时）。
    pub fn register(&self, inst: &str, tx: mpsc::Sender<TcpStream>) {
        self.map.insert(inst.to_string(), tx);
    }

    /// 注销（实例停止时）。
    pub fn unregister(&self, inst: &str) {
        self.map.remove(inst);
    }

    /// 是否有实例在等连接。
    pub fn contains(&self, inst: &str) -> bool {
        self.map.contains_key(inst)
    }

    /// 分发连接到实例 task。成功返回 `Ok(())`；失败归还连接 + 原因（acceptor 回 HelloAck 后 close）。
    pub fn try_dispatch(
        &self,
        inst: &str,
        conn: TcpStream,
    ) -> Result<(), (TcpStream, DispatchErr)> {
        if let Some(slot) = self.map.get(inst) {
            match slot.value().try_send(conn) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(c)) => Err((c, DispatchErr::Full)),
                Err(mpsc::error::TrySendError::Closed(c)) => Err((c, DispatchErr::NoSlot)),
            }
        } else {
            Err((conn, DispatchErr::NoSlot))
        }
    }
}

/// 注入给每个插件 task 的宿主能力。
///
/// `lux::EmbeddedClient` 未实现 `Debug`，故以 `expect` 放行 `missing_debug_implementations`
///（若未来 EmbeddedClient 补 Debug，编译器会提示移除该 expect）。
#[expect(missing_debug_implementations)]
#[derive(Clone)]
pub struct HostContext {
    /// 进程内 lux 客户端（零拷贝，Clone 共享 store/WAL）。
    pub lux: EmbeddedClient,
    /// 边缘端硬件 ID。
    pub hwid: crate::types::Hwid,
    /// 全局关闭信号。
    pub shutdown: CancellationToken,
    /// 命令路由表。
    pub ctl_registry: CtlRegistry,
    /// bridge 连接分发表（外部插件 sidecarClient）。
    pub bridge_registry: BridgeRegistry,
    /// 进程级指标（M4）。
    pub metrics: std::sync::Arc<crate::process_metrics::ProcessMetrics>,
    /// 报文 trace 开关注册表（P1 子系统 2）：per-instance 运行时激活。
    pub trace_registry: crate::io_trace::TraceRegistry,
    /// SSE 事件总线（阶段一）：生产者（log_ringbuffer / io_trace / instance emit_state）
    /// 投递事件，SSE endpoint 订阅。无消费者时 publish 静默零开销。
    pub event_tx: crate::event_bus::EventBus,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CtlMessage;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    fn registry() -> CtlRegistry {
        CtlRegistry::new(Arc::new(crate::process_metrics::ProcessMetrics::new()))
    }

    fn dummy() -> CtlMessage {
        CtlMessage {
            instance: "x".into(),
            devid: "d".into(),
            cmd: "write".into(),
            data: String::new(),
        }
    }

    #[tokio::test]
    async fn send_counts_and_returns_semantics_unchanged() {
        let r = registry();
        let (tx, mut rx) = mpsc::channel::<CtlMessage>(1);
        r.register("a", tx);
        // 未注册 → false + not_found++
        assert!(!r.send("nope", dummy()));
        assert_eq!(r.metrics.ctl_not_found_count.load(Ordering::Relaxed), 1);
        // 命中 + Ok → true + send++
        assert!(r.send("a", dummy()));
        assert!(rx.recv().await.is_some(), "命令应送达");
        assert_eq!(r.metrics.ctl_send_count.load(Ordering::Relaxed), 2);
        // 命中 + channel 满 → true + dropped++
        let (tx2, _rx2) = mpsc::channel::<CtlMessage>(1);
        r.register("full", tx2);
        assert!(r.send("full", dummy()), "满丢仍返 true（对标 Go default）");
        assert!(r.send("full", dummy()), "第二次满");
        assert_eq!(r.metrics.ctl_dropped_count.load(Ordering::Relaxed), 1);
    }
}
