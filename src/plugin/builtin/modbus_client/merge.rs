//! 批量合并（纯函数）：按 func 分组 + 贪心合并相邻/近邻点为读取区间。

use super::address::{Func, PointSpec};

/// 合并后的连续读取区间（owned；points 不借自输入，便于 `SlavePlan` 持有跨 tick）。
/// `Clone`：单测用 `merge_regions(...)[0].clone()` 构造 SlavePlan（避免重新解析地址串）。
#[derive(Debug, Clone)]
pub(super) struct Region {
    pub(super) func: Func,
    pub(super) start: u16,
    pub(super) end: u16,
    pub(super) points: Vec<(String, PointSpec)>,
}

impl Region {
    /// 区间地址单元数（`end - start + 1`；coil/discrete 为 bit 数，holding/input 为寄存器数）。
    pub(super) fn len(&self) -> u16 {
        self.end - self.start + 1
    }
}

/// 按 func 分组 + 组内按 addr 排序 + 贪心合并相邻/近邻点为读取区间。
///
/// 4 种 func 各自独立地址空间，必分 4 组。组内合并条件：`addr <= end + gap_threshold + 1`
/// （最大单空洞）且新区间长 < `max_regs`。同 addr 多 bit/多 bool 折叠进同一区间。
pub(super) fn merge_regions(pairs: &[(String, PointSpec)], gap: u16, max_regs: u16) -> Vec<Region> {
    let mut out = Vec::new();
    for func in [Func::Coil, Func::Discrete, Func::Holding, Func::Input] {
        let mut group: Vec<&(String, PointSpec)> =
            pairs.iter().filter(|(_, p)| p.func == func).collect();
        group.sort_by_key(|(_, p)| p.addr);
        let mut cur: Option<Region> = None;
        for (field, p) in group {
            let end_p = p.addr.saturating_add(p.width() - 1);
            let merged = matches!(&cur, Some(r)
                if p.addr <= r.end.saturating_add(gap).saturating_add(1)
                    && end_p.saturating_sub(r.start) < max_regs);
            if merged {
                let r = cur.as_mut().expect("checked Some above");
                r.end = r.end.max(end_p);
                r.points.push((field.clone(), p.clone()));
            } else {
                if let Some(r) = cur.take() {
                    out.push(r);
                }
                cur = Some(Region {
                    func,
                    start: p.addr,
                    end: end_p,
                    points: vec![(field.clone(), p.clone())],
                });
            }
        }
        if let Some(r) = cur.take() {
            out.push(r);
        }
    }
    out
}
