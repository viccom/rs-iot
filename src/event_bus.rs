//! 事件总线（SSE 实时推送 · 阶段一）：进程内 `tokio::sync::broadcast` 扇出。
//!
//! 3 类生产者经 `EventBus::publish` 投递，1 个 `GET /api/v1/events` SSE endpoint
//! 订阅。容量 256（对标 hub）。
//!
//! 设计要点：
//! - 用 `broadcast`（非 mpsc）：多 SSE 客户端各自 `subscribe()` 得独立 Receiver，
//!   生产者不阻塞、**不因满丢新事件**——慢客户端缓冲满则覆写其最旧未读值，该客户端
//!   下次 `recv()` 收 `Lagged`（SSE 端 `filter_map` 丢帧继续），不影响生产者与其他客户端。
//! - 无消费者（启动期 / 无 SSE 连接）→ `send` 返 `Err`，`publish` 静默，零开销。
//! - `EventPayload::Log` 用 `Arc<LogEntry>`：N 客户端共享同一份日志，避免深拷贝。
//!
//! 模块依赖：log_ringbuffer（`LogEntry`）与 event_bus（`EventPayload::Log`）双向引用，
//! crate 内合法（无循环 crate 依赖，仅模块间 use）。

use crate::log_ringbuffer::LogEntry;
use crate::plugin::instance::InstState;
use crate::types::HostKind;
use tokio::sync::broadcast;

/// SSE 推送的事件载荷。
///
/// `#[serde(flatten)]` 让 `Log` 变体展开 `LogEntry` 字段，与 `/api/v1/logs/recent`
/// 返回结构同构，前端复用同一渲染逻辑。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum EventPayload {
    /// 一条日志（tracing event 经 RingBufferLayer 截获）。
    Log {
        /// 展开为顶层字段（ts/level/target/message/fields），与 `/logs/recent` 同构。
        #[serde(flatten)]
        entry: std::sync::Arc<LogEntry>,
    },
    /// 一帧报文（io_trace dump：modbus/lua 收发字节 hex）。
    Packet {
        /// 实例 ID。
        inst_id: String,
        /// 方向："tx"（发送）/ "rx"（接收）。
        dir: &'static str,
        /// hex dump（大写空格分隔，如 "01 03 0A FF"）。
        hex: String,
        /// 时间戳（ISO8601/RFC3339）。
        ts: String,
    },
    /// 实例状态变更（Starting/Running/Stopping/Stopped/Failed）。
    State {
        /// 实例 ID。
        inst_id: String,
        /// 新状态。
        state: InstState,
        /// 插件 app_code（如 "modbusClient"）。
        app_code: String,
        /// 宿主类型（builtin/external）。
        host_kind: HostKind,
        /// 时间戳（ISO8601/RFC3339）。
        ts: String,
    },
    /// 一批 rtdata 变更通知（pipeline flush_batch 写入成功后发）。
    ///
    /// **批次级聚合**：一批（50ms 攒批窗口）一次事件，**不是每点一次**——50万点/秒下
    /// 每点 publish 会爆 SSE（broadcast 256 容量 Lagged 雪崩 + 50万次 JSON 序列化）。
    /// payload 只带"哪些 dev 变了 + 本批点数"，**不含 value**——前端收到后用现有 2s
    /// 轮询 `GET /rtdatas` 拉最新值。推送只作"刷新触发"，数据正确性靠轮询兜底。
    Rtdata {
        /// 本批变更的 devid 列表（去重；现场极端多设备时截断，count 仍标真实总量）。
        dev_ids: Vec<String>,
        /// 本批点数（供前端估算变更量级，未截断的真实值）。
        count: usize,
        /// 时间戳（ISO8601/RFC3339）。
        ts: String,
    },
}

impl EventPayload {
    /// SSE `event:` 头字段值（前端 `addEventListener(name)` 据此分发）。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Log { .. } => "log",
            Self::Packet { .. } => "packet",
            Self::State { .. } => "state",
            Self::Rtdata { .. } => "rtdata",
        }
    }
}

/// 当前 UTC 时间 RFC3339（ISO8601）。
///
/// 从 `log_ringbuffer` 提升为 crate 共用：log_ringbuffer 的 `on_event` 与 io_trace 的
/// `dump` 复用同一时间格式（与 `LogEntry.ts` 一致，前端 `fmtAge` 可解析）。
pub(crate) fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// 事件总线：包装 `broadcast::Sender`。
///
/// Clone 廉价（Sender 引用计数）。生产者持 Clone `publish()`，SSE endpoint 持 Clone
/// `subscribe()` 得独立 Receiver。无消费者时 `send` 返 `Err`，`publish` 静默零开销。
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<EventPayload>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("receiver_count", &self.tx.receiver_count())
            .finish()
    }
}

impl EventBus {
    /// 建总线（容量 `cap`，生产用 256 对标 hub）。
    pub fn new(cap: usize) -> Self {
        let (tx, _) = broadcast::channel(cap);
        Self { tx }
    }

    /// 测试用孤立 sender（无 receiver，`publish` 静默）。
    /// 生产构造点（HostContext / RingBufferLayer / AppState）测试时用此填字段。
    pub fn dummy() -> Self {
        Self::new(1)
    }

    /// 订阅（SSE endpoint 每连接调一次，得独立 Receiver）。
    pub fn subscribe(&self) -> broadcast::Receiver<EventPayload> {
        self.tx.subscribe()
    }

    /// 投递事件。
    ///
    /// 同步点（`RingBufferLayer::on_event` / `io_trace::dump` / `instance::emit_state`）
    /// 不能 await；`broadcast::send` 本身同步不阻塞——有 receiver 必成功（慢客户端
    /// 缓冲满则覆写其旧值、下次 recv 收 Lagged），无 receiver 返 `Err` 静默。
    pub fn publish(&self, payload: EventPayload) {
        let _ = self.tx.send(payload);
    }
}
