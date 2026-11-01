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
//! 模块依赖：载荷类型（`LogEntry`/`InstState`）定义在 `types` 共享内核——早期它们
//! 定义在生产者模块（`log_ringbuffer`/`plugin::instance`），导致 `event_bus` 反向
//! 引用生产方形成模块环（2026-11-01 分层耦合审查修复）。本模块只依赖 `types`。

use crate::types::{HostKind, InstState, LogEntry, SampleValue};
use std::collections::BTreeMap;
use tokio::sync::broadcast;

/// Rtdata 事件带的单设备最新值（Q6 带值扩展，C 阶段 T1）。
///
/// 由 pipeline `aggregate_rtdata` 从 50ms 攒批窗口聚合：同批同 dev 合并为一个
/// （tag 后写覆盖），`ts` 取该 dev 本批最后一笔时间戳。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RtdataSample {
    /// 设备 ID（与 `Rtdata::dev_ids` 元素同源、同步截断）。
    pub devid: String,
    /// 该 dev 本批最新值（tag 形如 "tag" 或 "devId.tag"，与 `Sample.values` 同源；
    /// `SampleValue` serde untagged——值直接序列化为 JSON 数字/布尔/字符串）。
    pub values: BTreeMap<String, SampleValue>,
    /// 该 dev 本批最后一笔的 Unix 秒时间戳。
    pub ts: i64,
}

/// 告警动作（Q12 告警最小闭环，spec §1.1）：fire=触发/更新，clear=清除。
///
/// serde 小写（`"fire"`/`"clear"`）——SSE JSON 直读，前端无映射表。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AlarmAction {
    /// 触发/更新（脚本判定成立；同 key 重复 fire 仍是本动作）。
    Fire,
    /// 清除（脚本判定恢复或 REST 手动清除；幂等语义由清除方保证）。
    Clear,
}

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
    /// Q6 带值扩展：`samples` 按 dev 聚合携带最新值（前端直用即时刷新）；lagged 丢帧 /
    /// dev 超限截断时数据不全，仍靠前端既有 2s 轮询 `GET /rtdatas` 兜底，推送只是加速。
    Rtdata {
        /// 本批变更的 devid 列表（去重；现场极端多设备时截断，count 仍标真实总量）。
        dev_ids: Vec<String>,
        /// 本批 Sample 条数（供前端估算变更量级，未截断的真实值；一个多 tag 的 Sample 记 1——与旧实现口径一致）。
        count: usize,
        /// 按 dev 聚合的最新值（与 dev_ids 同步截断；旧前端可忽略——缺失/不全时
        /// 轮询兜底不变，兼容字段零破坏）。`Arc` 包裹（复审 P2-4）：broadcast 要求
        /// `T: Clone`，裸 Vec 会给**每个接收者**（K 个 SSE + M 个订阅型实例）深克隆
        /// 整个载荷——Arc 后 fan-out 克隆降为引用计数（serde `rc` feature 透明序列化）。
        samples: std::sync::Arc<Vec<RtdataSample>>,
        /// 时间戳（ISO8601/RFC3339）。
        ts: String,
    },
    /// 一条告警事件（Q12 告警最小闭环，spec §1.1）。
    ///
    /// fire/clear 双向同一变体（`action` 区分）——前端一次 `addEventListener("alarm")`
    /// 覆盖两向，多客户端视图经 clear 帧收敛一致。`source` 由宿主从实例 ID 注入
    /// （脚本不可伪造）；发布方：脚本面 `ctx:fire_event`/`clear_event`（T4）+
    /// REST 手动清除。
    Alarm {
        /// 动作（fire/clear）。
        action: AlarmAction,
        /// 告警键（脚本自定义，如 "dev1.temp_high"）。
        key: String,
        /// 级别（"info"/"warn"/"error"；clear 沿用清除时条目的 level）。
        level: String,
        /// 来源实例 ID（宿主填，脚本不可伪造）。
        source: String,
        /// 人类可读描述（fire 时）。
        message: String,
        /// 时间戳（ISO8601/RFC3339，宿主填）。
        ts: String,
    },
    /// 画面/主题文档变更（P0 版本管理，design 2026-10-30-screen-versioning §3.5）。
    ///
    /// 发布方 routes_meta：put（**判重命中不发**——内容未变不该惊动运行页）/
    /// delete / rollback。`resource` 区分 screens/themes（同一事件 kind，机制
    /// 随 MetaKind 泛化免费覆盖 themes）。前端 `addEventListener("screen")` 感知
    /// 「画面已被他人/AI 修改」。
    Screen {
        /// 画面/主题 id。
        id: String,
        /// 资源种类（"screen" / "theme"）。
        resource: &'static str,
        /// 动作（"put" / "delete" / "rollback"）。
        action: &'static str,
        /// 变更后的版本号（delete = 删除前的 seq）。
        version: u64,
        /// 操作者（JWT sub；hmi-mcp 用专用 sub 天然可分辨人/AI）。
        actor: String,
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
            Self::Alarm { .. } => "alarm",
            Self::Screen { .. } => "screen",
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

    /// 当前接收者数（复审 P2-3）：生产侧（pipeline 聚合前）据此跳过无人消费的
    /// 聚合开销。竞态窗口（检查后新订阅者到达）一批可容忍——订阅者本就收不到
    /// 订阅前事件。
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
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
