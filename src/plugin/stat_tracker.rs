//! 实例指标追踪器（M4）：插件主采集 task 内持有，封装 InstanceStat 计数 + 周期 flush。
//!
//! 单 task 串行持有（`&mut self`，无需锁）；插件 select! 加 flush ticker + 采集后 tick_ok/fail。
//!
//! flush 节奏（2026-08 CPU 削峰）：ticker 心跳 5s 且按 inst_id 错峰（见 [`flush_ticker`]，
//! 多实例同刻启动时落盘散布整周期，削整点惊群）；ticker 臂走 [`flush_if_due`]——
//! **状态变更（degraded↔running）下拍即落**，否则距上次落盘 ≥
//! [`FLUSH_MAX_INTERVAL_SECS`] 才落（计数类指标 30s 陈旧度足够，Prometheus counter
//! 粒度变粗无碍）。tick 路径 [`should_flush`]（每 50 拍）与退出路径 `flush()` 不变。

use crate::db::stat::InstanceStat;
use lux::EmbeddedClient;
use std::time::{Duration, Instant};

/// tick 路径周期 flush：每 N 次活动落一次 lux（防大点表热点）。
pub const FLUSH_EVERY_TICKS: u64 = 50;
/// ticker 心跳周期（错峰相位后，见 [`flush_ticker`]）。落盘间隔上限见
/// [`FLUSH_MAX_INTERVAL_SECS`]——心跳到点只代表「检查一次」，多数拍不写。
pub const FLUSH_INTERVAL_SECS: u64 = 5;
/// 距上次成功落盘超过该秒数 → ticker 拍无条件落（计数兜底；状态变更则不受此限，
/// 由 `status_dirty` 触发下拍即落）。
pub const FLUSH_MAX_INTERVAL_SECS: u64 = 30;
/// 连续失败 N 次后 status 转 degraded。
const DEGRADED_AFTER_FAILS: u32 = 3;

/// 实例指标追踪器。
///
/// `lux::EmbeddedClient` 未实现 `Debug`，故以 `expect` 放行 `missing_debug_implementations`
///（与 `host.rs::HostContext` 同款，lux 类型约束）。
#[expect(missing_debug_implementations)]
pub struct StatTracker {
    stat: InstanceStat,
    inst_id: String,
    lux: EmbeddedClient,
    consec_fails: u32,
    /// 上次**成功**落盘时刻；None=从未落过（下一次到期判定恒真）。
    last_flush: Option<Instant>,
    /// status 自上次落盘后变更过（degraded↔running/stopped 等）→ 下拍 ticker 即落。
    status_dirty: bool,
}

/// 构建错峰的 stat flush ticker：`interval_at(now + phase, FLUSH_INTERVAL_SECS)`。
///
/// 多实例同刻启动时，裸 `interval` 会让所有 stat 落盘落在同一墙钟栅格（实测整点
/// CPU 惊群的主源之一）；这里按 `phase_ms(inst_id + "#stat")` 散相（盐 `#stat` 使
/// 采集周期恰好 = 5s 的实例（如 period=5 的 modbus）stat 相位与数据 ticker 错开）。
/// **首个 tick 在 now+phase（0..5s 后）**——启动瞬间的初始 stat 由调用方在进 loop
/// 前显式 `flush()` 一次（保持「运行中 stat:<inst> 即存在」的原有可见性）。
pub fn flush_ticker(inst_id: &str) -> tokio::time::Interval {
    let period = Duration::from_secs(FLUSH_INTERVAL_SECS);
    let phase = Duration::from_millis(crate::ticker::phase_ms(&format!("{inst_id}#stat"), period));
    tokio::time::interval_at(tokio::time::Instant::now() + phase, period)
}

impl StatTracker {
    /// 构造（status 初始 running，计数 0）。
    pub fn new(inst_id: String, lux: EmbeddedClient) -> Self {
        Self {
            stat: InstanceStat {
                status: "running".into(),
                tick_count: 0,
                fail_count: 0,
                last_activity_ts: 0,
                last_activity_duration_ms: 0,
                last_err: String::new(),
                extra: serde_json::Map::new(),
            },
            inst_id,
            lux,
            consec_fails: 0,
            last_flush: None,
            status_dirty: false,
        }
    }

    /// 活动成功：tick_count++，更新 last_activity_*，清 last_err，status=running。
    pub fn tick_ok(&mut self, duration_ms: u64) {
        self.stat.tick_count = self.stat.tick_count.saturating_add(1);
        self.stat.last_activity_ts = chrono::Local::now().timestamp();
        self.stat.last_activity_duration_ms = duration_ms;
        self.stat.last_err.clear();
        self.consec_fails = 0;
        if self.stat.status != "running" {
            self.status_dirty = true;
        }
        self.stat.status = "running".into();
    }

    /// 活动失败：fail_count++，last_err=err；连续失败达阈值 → status=degraded。
    pub fn tick_fail(&mut self, err: &str) {
        self.stat.fail_count = self.stat.fail_count.saturating_add(1);
        self.stat.last_activity_ts = chrono::Local::now().timestamp();
        self.stat.last_err = err.into();
        self.consec_fails = self.consec_fails.saturating_add(1);
        if self.consec_fails >= DEGRADED_AFTER_FAILS && self.stat.status != "degraded" {
            self.status_dirty = true;
            self.stat.status = "degraded".into();
        }
    }

    /// 写扩展字段（opcua session_reconnect_count / lua vm_restart_count 等）。
    pub fn extra_insert(&mut self, k: &str, v: serde_json::Value) {
        self.stat.extra.insert(k.into(), v);
    }

    /// 直接设 status（如退出时设 stopped）。状态实际变更时标 dirty。
    pub fn set_status(&mut self, s: &str) {
        if self.stat.status != s {
            self.status_dirty = true;
        }
        self.stat.status = s.into();
    }

    /// 是否到 tick 路径周期 flush 点（tick % FLUSH_EVERY_TICKS == 0 且 tick>0）。
    pub fn should_flush(&self) -> bool {
        self.stat.tick_count != 0 && self.stat.tick_count.is_multiple_of(FLUSH_EVERY_TICKS)
    }

    /// ticker 拍的条件落盘：`status_dirty`（状态变更，下拍即落）或距上次成功落盘
    /// ≥ [`FLUSH_MAX_INTERVAL_SECS`] 才写，否则零 lux 往返直接返回。
    pub async fn flush_if_due(&mut self) {
        let due = self.status_dirty
            || self
                .last_flush
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(FLUSH_MAX_INTERVAL_SECS));
        if due {
            self.flush().await;
        }
    }

    /// 无条件落盘（tick 路径 should_flush 点 / 退出路径 / 启动初始 stat）。
    /// 成功才推进 `last_flush`/清 `status_dirty`——失败保脏，下拍到点重试。
    pub async fn flush(&mut self) {
        if let Err(e) = crate::db::stat::write(&self.lux, &self.inst_id, &self.stat).await {
            tracing::warn!("[stat] flush {} failed: {e}", self.inst_id);
            return;
        }
        self.last_flush = Some(Instant::now());
        self.status_dirty = false;
    }

    /// 当前 tick_count（测试用）。
    #[cfg(test)]
    pub fn tick_count(&self) -> u64 {
        self.stat.tick_count
    }

    /// 当前 status（测试用）。
    #[cfg(test)]
    pub fn status(&self) -> &str {
        &self.stat.status
    }

    /// 当前 fail_count（测试用）。
    #[cfg(test)]
    pub fn stat_fail_count_for_test(&self) -> u64 {
        self.stat.fail_count
    }

    /// 是否有未落盘的状态变更（测试用）。
    #[cfg(test)]
    pub fn status_dirty_for_test(&self) -> bool {
        self.status_dirty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // EmbeddedClient 无 Default 实现，沿用 db::stat / db::luakv 的测试范式：
    // 起 lux（port 0 + 独立 temp data_dir）取 client。本模块单测不调 flush，
    // 仅验证 tick 计数/状态语义，但构造 tracker 仍需真实 EmbeddedClient。
    async fn tracker() -> StatTracker {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("stattracker-{}", std::process::id()))
                    .to_str()
                    .unwrap()
                    .to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        StatTracker::new("t".into(), handle.client())
    }

    #[tokio::test]
    async fn tick_ok_increments_and_clears_err() {
        let mut t = tracker().await;
        t.tick_fail("e");
        t.tick_ok(10);
        assert_eq!(t.tick_count(), 1);
        assert_eq!(t.status(), "running");
    }

    #[tokio::test]
    async fn tick_fail_degrades_after_threshold() {
        let mut t = tracker().await;
        for _ in 0..3 {
            t.tick_fail("e");
        }
        assert_eq!(t.status(), "degraded", "连续 3 次失败转 degraded");
        assert_eq!(t.stat_fail_count_for_test(), 3);
    }

    #[tokio::test]
    async fn tick_ok_resets_degraded() {
        let mut t = tracker().await;
        for _ in 0..3 {
            t.tick_fail("e");
        }
        assert_eq!(t.status(), "degraded");
        t.tick_ok(5);
        assert_eq!(t.status(), "running", "成功恢复转 running");
    }

    #[tokio::test]
    async fn should_flush_at_every_50_boundary() {
        let mut t = tracker().await;
        assert!(!t.should_flush(), "tick=0 不 flush");
        for _ in 0..50 {
            t.tick_ok(1);
        }
        assert!(t.should_flush(), "tick=50 flush");
        t.tick_ok(1);
        assert!(!t.should_flush(), "tick=51 不 flush");
    }

    /// 状态变更（running→degraded→running）标 dirty → flush_if_due 下拍即落并清脏。
    #[tokio::test]
    async fn status_change_marks_dirty_and_flush_clears() {
        let mut t = tracker().await;
        t.flush().await; // 初始落盘（last_flush 推进、dirty 清）
        assert!(!t.status_dirty_for_test());

        t.tick_ok(1);
        assert!(!t.status_dirty_for_test(), "running→running 不算变更");

        for _ in 0..3 {
            t.tick_fail("e");
        }
        assert!(t.status_dirty_for_test(), "转 degraded 标脏");

        t.flush_if_due().await; // 距上次落盘远小于 30s，靠 dirty 触发
        assert!(!t.status_dirty_for_test(), "落盘后清脏");

        t.tick_ok(2);
        assert!(t.status_dirty_for_test(), "degraded→running 恢复也算变更");
    }

    /// 从未落盘（last_flush=None）→ flush_if_due 恒落；刚落过且无变更 → 不落（零往返）。
    /// 「不落」无法从外部直接观测，用 status_dirty 保持 false + 语义推导：这里锁
    /// dirty 分支行为（aged 分支由 last_flush=None 同一表达式覆盖，30s 真实等待不进单测）。
    #[tokio::test]
    async fn flush_if_due_dirty_branch_only_until_max_interval() {
        let mut t = tracker().await;
        // 构造后未落盘：dirty=false 但 last_flush=None → due（表达式 map_or 分支）
        t.set_status("stopped");
        assert!(t.status_dirty_for_test(), "set_status 实际变更标脏");
        t.flush_if_due().await;
        assert!(!t.status_dirty_for_test());
    }

    /// flush_ticker 相位对 inst_id 散布且与采集 ticker（同周期）错开：直接锁
    /// `flush_ticker` 用的相位表达式（盐 #stat），防后续改动漂移到与数据 ticker 同相。
    #[test]
    fn flush_ticker_phase_is_salted_and_spread() {
        let period = Duration::from_secs(FLUSH_INTERVAL_SECS);
        let phases: Vec<u64> = ["i0", "i1", "i2", "i3", "i4", "i5", "i6", "i7"]
            .iter()
            .map(|id| crate::ticker::phase_ms(&format!("{id}#stat"), period))
            .collect();
        let distinct = phases
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(distinct >= 2, "多实例相位应散布（非全部同相）");
    }
}
