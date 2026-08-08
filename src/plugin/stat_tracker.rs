//! 实例指标追踪器（M4）：插件主采集 task 内持有，封装 InstanceStat 计数 + 周期 flush。
//!
//! 单 task 串行持有（`&mut self`，无需锁）；插件 select! 加 flush ticker + 采集后 tick_ok/fail。

use crate::db::stat::InstanceStat;
use lux::EmbeddedClient;

/// 周期 flush：每 N 次活动落一次 lux（防大点表热点）。
pub const FLUSH_EVERY_TICKS: u64 = 50;
/// 周期 flush：每 N 秒落一次 lux（与 FLUSH_EVERY_TICKS 取先到）。
pub const FLUSH_INTERVAL_SECS: u64 = 5;
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
        }
    }

    /// 活动成功：tick_count++，更新 last_activity_*，清 last_err，status=running。
    pub fn tick_ok(&mut self, duration_ms: u64) {
        self.stat.tick_count = self.stat.tick_count.saturating_add(1);
        self.stat.last_activity_ts = chrono::Local::now().timestamp();
        self.stat.last_activity_duration_ms = duration_ms;
        self.stat.last_err.clear();
        self.consec_fails = 0;
        self.stat.status = "running".into();
    }

    /// 活动失败：fail_count++，last_err=err；连续失败达阈值 → status=degraded。
    pub fn tick_fail(&mut self, err: &str) {
        self.stat.fail_count = self.stat.fail_count.saturating_add(1);
        self.stat.last_activity_ts = chrono::Local::now().timestamp();
        self.stat.last_err = err.into();
        self.consec_fails = self.consec_fails.saturating_add(1);
        if self.consec_fails >= DEGRADED_AFTER_FAILS {
            self.stat.status = "degraded".into();
        }
    }

    /// 写扩展字段（opcua session_reconnect_count / lua vm_restart_count 等）。
    pub fn extra_insert(&mut self, k: &str, v: serde_json::Value) {
        self.stat.extra.insert(k.into(), v);
    }

    /// 直接设 status（如退出时设 stopped）。
    pub fn set_status(&mut self, s: &str) {
        self.stat.status = s.into();
    }

    /// flush 到 lux stat:<inst>（失败仅 warn，不阻塞采集）。
    pub async fn flush(&self) {
        if let Err(e) = crate::db::stat::write(&self.lux, &self.inst_id, &self.stat).await {
            tracing::warn!("[stat] flush {} failed: {e}", self.inst_id);
        }
    }

    /// 是否到周期 flush 点（tick % FLUSH_EVERY_TICKS == 0 且 tick>0）。
    pub fn should_flush(&self) -> bool {
        self.stat.tick_count != 0 && self.stat.tick_count % FLUSH_EVERY_TICKS == 0
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
}
