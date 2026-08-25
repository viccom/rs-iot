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

/// 单写线程：recv 攒批 + deadband/去重/防混型 → tsink insert_rows（D3/D4/D5）。
pub mod writer;
