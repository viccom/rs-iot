//! R2 频控（设计 §4.5-2/Q3）：每实例每秒窗口计数器——原子计数 + Mutex 窗口起点
//!（无 async 锁，check 瞬时返回不跨 await）。超限返回 false，由调用方转脚本可感知
//! 错误（`nil, "rate limited"` 语义，可退避）。限值单一事实源在能力卡
//! [`crate::plugin::lua_core::capability`] 的 `rate_per_sec`（当前 http_get/post 20/s）。

/// 每秒窗口计数器（1s 窗口，窗口过期整体重置）。
///
/// 计划骨架语义（T5）：`check()` 放行窗口内前 `limit` 次，超出返回 false；
/// 窗口起点惰性重置（首个过期后的 check 归零开新窗）。
pub(crate) struct RateLimiter {
    /// 窗口内放行上限（次/秒）。
    limit: u32,
    /// 窗口长度（1s）。
    window: std::time::Duration,
    /// 当前窗口已用次数。
    cur: std::sync::atomic::AtomicU32,
    /// 当前窗口起点（Mutex 保护重置与计数的窗口一致性）。
    window_start: std::sync::Mutex<std::time::Instant>,
}

impl RateLimiter {
    /// 新建限流器：`limit` 次 / 1s 窗口。
    pub(crate) fn new(limit: u32) -> Self {
        Self {
            limit,
            window: std::time::Duration::from_secs(1),
            cur: std::sync::atomic::AtomicU32::new(0),
            window_start: std::sync::Mutex::new(std::time::Instant::now()),
        }
    }

    /// 本次调用是否放行（窗口内第 1..=limit 次 true，超出 false）。
    ///
    /// 窗口过期则先归零开新窗（起点持锁期间重置 + 判定，窗口口径一致）；
    /// `fetch_add` 返回自增值的前值，`前值 < limit` 即本次在额度内。
    pub(crate) fn check(&self) -> bool {
        let mut start = self
            .window_start
            .lock()
            .expect("ratelimit window_start 锁中毒（check 内无 panic 路径，不可达）");
        if start.elapsed() >= self.window {
            self.cur.store(0, std::sync::atomic::Ordering::Relaxed);
            *start = std::time::Instant::now();
        }
        self.cur.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 计划 T5 Step 1：3 次内放行，第 4 次拒。
    #[test]
    fn allows_up_to_limit_then_blocks() {
        let rl = RateLimiter::new(3);
        assert!(rl.check() && rl.check() && rl.check());
        assert!(!rl.check(), "第 4 次应被限流");
    }
}
