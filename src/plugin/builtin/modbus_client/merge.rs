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

/// 单区间单元数上限（MB-6，2026-11-01 协议语义审查）：寄存器类 FC03/04 协议上限
/// 125；coil/discrete 走 FC01/02，协议上限 2000。`max_regs` 是用户配置（可低于
/// 协议上限），实际合并取两者较小值——此前统一 clamp 125 把大布尔点表多切成数十个请求。
pub(super) fn func_max_units(f: Func) -> u16 {
    match f {
        Func::Holding | Func::Input => 125,
        Func::Coil | Func::Discrete => 2000,
    }
}

/// 按 func 分组 + 组内按 addr 排序 + 贪心合并相邻/近邻点为读取区间。
///
/// 4 种 func 各自独立地址空间，必分 4 组。组内合并条件：`addr <= end + gap_threshold + 1`
/// （最大单空洞）且新区间长 < `max_regs`（再受 [`func_max_units`] 协议上限约束）。
/// 同 addr 多 bit/多 bool 折叠进同一区间。
pub(super) fn merge_regions(pairs: &[(String, PointSpec)], gap: u16, max_regs: u16) -> Vec<Region> {
    let mut out = Vec::new();
    for func in [Func::Coil, Func::Discrete, Func::Holding, Func::Input] {
        let mut group: Vec<&(String, PointSpec)> =
            pairs.iter().filter(|(_, p)| p.func == func).collect();
        group.sort_by_key(|(_, p)| p.addr);
        let cap = max_regs.min(func_max_units(func));
        let mut cur: Option<Region> = None;
        for (field, p) in group {
            let end_p = p.addr.saturating_add(p.width() - 1);
            let merged = matches!(&cur, Some(r)
                if p.addr <= r.end.saturating_add(gap).saturating_add(1)
                    && end_p.saturating_sub(r.start) < cap);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::builtin::modbus_client::address::parse_point;

    /// MB-6（2026-11-01 协议语义审查）：coil/discrete 单请求上限是 2000（FC01/02），
    /// 寄存器类是 125（FC03/04）——此前统一 clamp 125，大布尔点表被切成多请求。
    #[test]
    fn merge_regions_caps_max_units_per_func() {
        // 2000 个连续 coil + max_regs=2000 → 1 个 2000 bit 区间（协议上限内）
        let coils: Vec<(String, PointSpec)> = (0..2000)
            .map(|i| {
                (
                    format!("c{i}"),
                    parse_point(&format!("coil:{i}:bool")).unwrap(),
                )
            })
            .collect();
        let r = merge_regions(&coils, 16, 2000);
        assert_eq!(r.len(), 1, "2000 coil 应合成 1 个区间");
        assert_eq!(r[0].len(), 2000);

        // 同样 max_regs=2000 的寄存器点位仍被钳 125：200 个 → 125 + 75 两段
        let regs: Vec<(String, PointSpec)> = (0..200)
            .map(|i| {
                (
                    format!("h{i}"),
                    parse_point(&format!("holding:{i}:u16")).unwrap(),
                )
            })
            .collect();
        let r = merge_regions(&regs, 16, 2000);
        assert_eq!(r.len(), 2, "寄存器类上限 125 → 200 点分 2 段");
        assert_eq!(r[0].len(), 125);
        assert_eq!(r[1].len(), 75);
        assert!(
            r.iter().all(|r| r.len() <= 125),
            "寄存器类单区间不得超过 125"
        );

        // 用户配置低于协议上限时按其值（coil 也一样）
        let r = merge_regions(&coils[..100], 16, 100);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].len(), 100);
    }
}
