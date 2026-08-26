//! 历史时序存储写路径（Phase 1b）：pipeline flush_batch 出口旁路 fan-out →
//! 有界 channel（满即丢）→ 单写线程 → tsink 内嵌存储。
//!
//! 三层开关：L1 = Cargo feature `history`（engine/writer 的编译期裁剪，tsink 为
//! 可选 path 依赖）；L2 = `[history].enabled`（部署期，`config.rs::HistoryConfig`）；
//! L3 = `TagProps.history`（per-tag 白名单，model effective 缓存）。设计与任务
//! 拆分见 `docs/designs/2026-08-25-history-write-path-plan.md`。

/// tsink 引擎组装与生命周期：StorageBuilder 资源边界组装（D6）、init/close（D2/D10）。
#[cfg(feature = "history")]
pub mod engine;

/// fan-out 出口类型（HistoryRow/HistoryStats/HistoryHandle）与白名单过滤——
/// 刻意不含 tsink 类型，P6 无条件编译，热路径纯 Rust 可测。
pub mod rows;

/// 出口类型与 fan_out 的门面重导出（Task 5）：pipeline/host 调用点写
/// `history::HistoryHandle`/`history::fan_out`，不必带 `rows::` 路径段。
pub use rows::{fan_out, HistoryHandle};

/// 单写线程：recv 攒批 + deadband/去重/防混型 → tsink insert_rows（D3/D4/D5）。
pub mod writer;

// ---------- 读路径公共类型（Phase 2 Task 1，P1 无条件编译） ----------
//
// trait 与类型只依赖 `SampleValue` 等 rs-iot 自有类型（同 1b P6 先例：rows.rs
// 无条件编译），impl 藏 feature 门控的 engine.rs——AppState 持
// `Option<Arc<dyn HistoryReader>>` 零 cfg 散布，feature off 构建不拉 tsink。

use crate::types::SampleValue;

/// 历史回读单点：毫秒时间戳 + 四型采样值（REST 层换算秒出；`value` 经
/// `SampleValue` 的 untagged serde，JSON 直接是数字/布尔/字符串）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct HistoryPoint {
    /// 毫秒时间戳（tsink 存储精度，写路径 D6 锚点）。
    pub ts_ms: i64,
    /// 采样值（Float/Int/Bool/Str 四型，写路径 lane 保证无 U64/Bytes）。
    pub value: SampleValue,
}

/// 降采样聚合函数（P3 六值，对齐 tsink 聚合推荐面；不可与整窗聚合混用——
/// 有 agg 必有 interval，REST 层联动校验）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HistoryAgg {
    /// 最小值。
    Min,
    /// 最大值。
    Max,
    /// 平均值。
    Avg,
    /// 求和。
    Sum,
    /// 计数。
    Count,
    /// 桶内最后值。
    Last,
}

/// 历史查询（P1/P3）：半开窗 [start_ms, end_ms) + 可选降采样 + 行数上限。
#[derive(Debug, Clone)]
pub struct HistoryQuery {
    /// 窗口起点（毫秒，含）。
    pub start_ms: i64,
    /// 窗口终点（毫秒，不含）。
    pub end_ms: i64,
    /// 降采样参数：(interval_ms, agg)；`None` 即 raw 读。
    pub downsample: Option<(i64, HistoryAgg)>,
    /// 返回点数上限（取最旧 n，tsink 语义；REST 层缺省 10000、上限 100000）。
    pub limit: usize,
}

/// 历史读抽象（Phase 2 Task 1）：REST 层经 [`crate::api::AppState::history_reader`]
/// 持 `Arc<dyn HistoryReader>`，真 impl 在 feature 门控的 engine.rs（测试侧
/// FakeReader 同面注入）。
///
/// - 参数序 `(metric, devid, q)`：metric=tag 名、devid=设备 ID（series labels
///   是写路径契约 `[devid, hwid]` 的前段，hwid 由 impl 侧补齐语义）；
/// - `Err(String)` 供 REST 层做 400/500 分层映射（P12：参数类错误串 → 400，
///   引擎类 → 500）；
/// - 同步方法：长窗 raw 扫描可能 >1ms，调用方（routes_history）以
///   `spawn_blocking` 包裹（P6）。
pub trait HistoryReader: Send + Sync {
    /// 查询单 series 的历史点（空窗/不存在 series 返回 `Ok(vec![])` 而非错误
    /// ——tsink 语义）。
    fn select(
        &self,
        metric: &str,
        devid: &str,
        q: &HistoryQuery,
    ) -> Result<Vec<HistoryPoint>, String>;
}

/// 历史引擎资源水位（Phase 3 治理，P4 无条件编译）：tsink 引擎内部口径
/// 外露——D6 恒设 memory_limit 故记账常开，读数纯原子。impl 藏 feature
/// 门控的 engine.rs（同 [`HistoryReader`] 先例），AppState 持
/// `Option<Arc<dyn HistoryTelemetry>>` 零 cfg 散布，feature off 构建不拉 tsink。
pub trait HistoryTelemetry: Send + Sync {
    /// 内存水位（tsink `memory_used()` 口径，字节；记账常开时读数纯原子）。
    fn memory_used_bytes(&self) -> u64;
    /// 内存预算（D6 `memory_limit_mb` 换算字节，即配即读）。
    fn memory_budget_bytes(&self) -> u64;
    /// series 数（`list_metrics().len()` 官方口径，O(series) 分配——调用方
    /// 以 `spawn_blocking` 包裹，P5；引擎关闭后 Err 按 0，不 panic）。
    fn series_count(&self) -> u64;
}

// ---------- 编排注入件（审计 B7：lib.rs 引擎知识收敛） ----------
//
// 原 lib.rs 以 ~60 行双 cfg 块 + 三态推导拼装五个注入件（handle/reader/
// telemetry/stats/state）——引擎内部知识泄漏进编排层。本结构一次产出，
// lib.rs 只取件；引擎所有权留在调用方（D10 关停序需要 `&mut engine.close`）。

use std::sync::Arc;

/// 引擎编排注入件（AppState 四槽 + HostContext.history + 三态字符串）。
/// （Debug 省略：trait object 字段无 Debug，同 AppState/HostContext expect 惯例。）
#[expect(missing_debug_implementations)]
pub struct EngineParts {
    /// HostContext.history（pipeline fan_out 出口；fail-soft None）。
    pub handle: Option<HistoryHandle>,
    /// AppState.history_reader（Phase 2 读路径）。
    pub reader: Option<Arc<dyn HistoryReader>>,
    /// AppState.history_telemetry（Phase 3 治理观测）。
    pub telemetry: Option<Arc<dyn HistoryTelemetry>>,
    /// AppState.history_stats（治理计数，与 handle 同源）。
    pub stats: Option<Arc<rows::HistoryStats>>,
    /// 三态："ok" | "init_failed" | "disabled"。
    pub state: Arc<str>,
}

impl EngineParts {
    /// feature off 恒 disabled 全 None（P7 enabled=true 告警由调用点）。
    pub fn disabled() -> Self {
        Self {
            handle: None,
            reader: None,
            telemetry: None,
            stats: None,
            state: Arc::from("disabled"),
        }
    }

    /// feature on：从 init 结果提取五件套并推导三态（原 lib.rs 逻辑原样
    /// 收敛——Some(stats) 与 "ok" 由同一 engine 来源成对产出）。
    #[cfg(feature = "history")]
    pub fn of(
        engine: Option<&engine::HistoryEngine>,
        cfg: &crate::config::HistoryConfig,
        hwid: &str,
    ) -> Self {
        let handle = engine.and_then(|e| e.handle());
        let stats = handle.as_ref().map(|h| h.stats.clone());
        Self {
            reader: engine.map(|e| e.reader(hwid)),
            telemetry: engine.map(|e| e.telemetry()),
            handle,
            stats,
            state: if !cfg.enabled {
                Arc::from("disabled")
            } else if engine.is_some() {
                Arc::from("ok")
            } else {
                Arc::from("init_failed")
            },
        }
    }
}
