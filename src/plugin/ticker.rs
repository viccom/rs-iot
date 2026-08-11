//! 采集 ticker jitter helper:FNV-1a(inst_id) % period_ms 确定性相位,
//! 让多实例 ticker 首次 tick 散布开,避免整点同步触发脉冲。

use std::time::Duration;

/// FNV-1a 64bit(确定性,无外部依赖)。
///
/// `pub(crate)` 供 `pipeline::route` 复用(DRY:devid 分片与 inst_id 抖动同算法,
/// 任一方改算法另一方自动跟随,无复制漂移)。
pub(crate) fn fnv1a_64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 返回首次 tick 的 jitter 相位(毫秒数):`fnv1a(inst_id) % period`。
/// 同 inst_id + 同 period → 完全相同(确定性);不同 inst_id → 散布在 [0, period)。
#[must_use]
pub(crate) fn phase_ms(inst_id: &str, period: Duration) -> u64 {
    let period_ms = period.as_millis().max(1) as u64;
    fnv1a_64(inst_id) % period_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn phase_deterministic_same_input() {
        let p = Duration::from_secs(1);
        assert_eq!(phase_ms("instA", p), phase_ms("instA", p));
    }
    #[test]
    fn phase_spreads_across_instances() {
        let p = Duration::from_secs(1);
        let phases: Vec<u64> = ["i0", "i1", "i2", "i3", "i4", "i5", "i6", "i7"]
            .iter()
            .map(|s| phase_ms(s, p))
            .collect();
        // 8 个实例,相位应散布(至少 4 个不同值,非全相同)
        let unique: std::collections::HashSet<u64> = phases.iter().copied().collect();
        assert!(unique.len() >= 4, "相位应散布,实际 unique={unique:?}");
        // 全部在 [0, 1000)
        assert!(phases.iter().all(|&x| x < 1000));
    }
    #[test]
    fn phase_zero_period_no_panic() {
        // period=0 → max(1) 兜底 → % 1 = 0,不 panic,退化为无 jitter
        assert_eq!(phase_ms("instA", Duration::ZERO), 0);
    }
}
