//! 实时数据订阅端点（OPC UA 式订阅语义；设计见
//! docs/designs/2026-08-23-rtdata-subscribe-plan.md）。
//!
//! `GET /api/v1/rtdatas/subscribe?dev_id=a,b&tag=x&interval=1000` → SSE：
//! - **snapshot**：连接建立即逐设备推全量快照（HGETALL `rtdata:<dev>`，一设备一帧）；
//! - **datachange**：之后每 `interval`（默认 1s，clamp [100ms,60s]）重读订阅范围，
//!   与上次已推值比对（仅比 cell[2] 值串——ts 变化值不变不算），有变化/删除才推一帧。
//!
//! 机制取舍：**每连接读侧 poll-diff，不走 EventBus**——绕过 pipeline 聚合的
//! `RTDATA_NOTIFY_MAX_DEVS=50` 截断与 broadcast 容量 256 的 Lagged 丢帧（订阅定位
//! 外部客户端可靠数据面）；interval 天然 per-connection（对标 OPC UA publishing
//! interval）；断线重连即重新 snapshot 全量重同步。pipeline / event_bus 零改动
//! ——现有 `/api/v1/events` 与 lua_transform 订阅面不受影响。

use crate::api::routes_rtdata::tag_of;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use tokio_stream::wrappers::ReceiverStream;

/// 订阅周期下限 ms（防高频滥用——每连接每 tick 全量 HGETALL 的成本护栏）。
const MIN_INTERVAL_MS: u64 = 100;
/// 订阅周期上限 ms。
const MAX_INTERVAL_MS: u64 = 60_000;
/// 默认订阅周期 ms。
const DEFAULT_INTERVAL_MS: u64 = 1_000;

/// query 参数（均可选）。
#[derive(Deserialize)]
pub struct SubQuery {
    /// 设备 ID 逗号列表（精确匹配；缺省 = 全部设备，每 tick 重查 KEYS，新设备自动纳入）。
    pub dev_id: Option<String>,
    /// tag 模糊过滤（field 末段 contains；语义同 `GET /api/v1/rtdatas?tag=`）。
    pub tag: Option<String>,
    /// 订阅周期 ms（clamp [100, 60000]，缺省 1000）。
    pub interval: Option<u64>,
}

/// interval clamp：缺省 DEFAULT，夹到 [MIN, MAX]。纯函数便于单测。
pub fn clamp_interval(raw: Option<u64>) -> u64 {
    raw.unwrap_or(DEFAULT_INTERVAL_MS)
        .clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS)
}

/// dev_id 逗号列表 → 去重集合。None/空白/全空段 → None（= 全部设备）。
pub fn parse_dev_ids(raw: Option<&str>) -> Option<HashSet<String>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let set: HashSet<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if set.is_empty() { None } else { Some(set) }
}

/// cell（`[ts, localTime, value]`）→ 值串（diff 比较键）。非数组/越界/非串 → 整体
/// JSON 串兜底（防畸形 cell 误判恒等）。
pub fn value_str(cell: &serde_json::Value) -> String {
    cell.get(2)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| cell.to_string())
}

/// 全订阅范围 diff：`current` = 本 tick 过滤后的 field → (值串, cell)；
/// `last_sent` = 已推基线 field → 值串。返回 (变更 field → cell, 消失 field 列表)。
///
/// **值串比较**——ts/localTime 变化值不变不算变更（对标 OPC UA DataChangeFilter
/// 的值触发语义；采集插件周期写回同值不产生通知）。
pub fn diff_scope(
    current: HashMap<String, (String, serde_json::Value)>,
    last_sent: &HashMap<String, String>,
) -> (serde_json::Map<String, serde_json::Value>, Vec<String>) {
    let mut now: HashSet<&str> = HashSet::with_capacity(current.len());
    let mut changes = serde_json::Map::new();
    for (field, (val, cell)) in &current {
        now.insert(field.as_str());
        if last_sent.get(field) != Some(val) {
            changes.insert(field.clone(), cell.clone());
        }
    }
    let removed: Vec<String> = last_sent
        .keys()
        .filter(|f| !now.contains(f.as_str()))
        .cloned()
        .collect();
    (changes, removed)
}

/// tag contains 过滤（field 末段匹配，复用 `GET /rtdatas` 的 tag_of 语义）。None 全保留。
pub fn filter_by_tag(
    m: serde_json::Map<String, serde_json::Value>,
    tag: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    match tag {
        None => m,
        Some(t) => m
            .into_iter()
            .filter(|(k, _)| tag_of(k).contains(t))
            .collect(),
    }
}

/// `GET /api/v1/rtdatas/subscribe`：OPC UA 式实时数据订阅（SSE）。
///
/// T3 骨架：空流（headers + 立即结束）；T4 起挂 producer（snapshot + 周期 diff）。
/// 客户端断开 → mpsc send 失败 → producer task 退出（资源回收）。
pub async fn subscribe() -> impl IntoResponse {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(8);
    drop(tx);
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn clamp_interval_defaults_and_bounds() {
        assert_eq!(clamp_interval(None), DEFAULT_INTERVAL_MS);
        assert_eq!(clamp_interval(Some(0)), MIN_INTERVAL_MS);
        assert_eq!(clamp_interval(Some(50)), MIN_INTERVAL_MS);
        assert_eq!(clamp_interval(Some(2000)), 2000);
        assert_eq!(clamp_interval(Some(u64::MAX)), MAX_INTERVAL_MS);
    }

    #[test]
    fn parse_dev_ids_none_empty_or_all_blank() {
        assert!(parse_dev_ids(None).is_none());
        assert!(parse_dev_ids(Some("")).is_none());
        assert!(parse_dev_ids(Some("  ")).is_none());
        assert!(parse_dev_ids(Some(",,")).is_none(), "全空段 = None（全部设备）");
    }

    #[test]
    fn parse_dev_ids_splits_trims_and_dedups() {
        let s = parse_dev_ids(Some("devA, devB ,devA")).unwrap();
        assert_eq!(s.len(), 2, "去重");
        assert!(s.contains("devA") && s.contains("devB"), "trim 生效");
    }

    #[test]
    fn value_str_takes_third_element() {
        assert_eq!(value_str(&json!([1, "t", "1.5"])), "1.5");
        assert_eq!(value_str(&json!([1, "t", "true"])), "true");
    }

    #[test]
    fn value_str_malformed_falls_back_to_json() {
        // 越界（无第三元素）→ 整体 JSON 串兜底，而非空串误判恒等。
        assert_eq!(value_str(&json!([1, "t"])), json!([1, "t"]).to_string());
        // 非数组 → 同上。
        assert_eq!(value_str(&json!("x")), "\"x\"");
    }

    /// 构造 diff 输入：field → (值串, cell)。
    fn scope(pairs: &[(&str, &str)]) -> HashMap<String, (String, serde_json::Value)> {
        pairs
            .iter()
            .map(|(f, v)| ((*f).to_string(), ((*v).to_string(), json!([1, "lt", v]))))
            .collect()
    }

    /// 构造已推基线：field → 值串。
    fn baseline(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(f, v)| ((*f).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn diff_detects_value_change_and_new_field() {
        let last = baseline(&[("d.t1", "1"), ("d.t2", "2")]);
        let cur = scope(&[("d.t1", "9"), ("d.t2", "2"), ("d.t3", "3")]);
        let (changes, removed) = diff_scope(cur, &last);
        assert_eq!(changes.len(), 2, "t1 值变 + t3 新增");
        assert!(changes.contains_key("d.t1") && changes.contains_key("d.t3"));
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_same_value_new_ts_not_a_change() {
        // OPC UA 语义核心：值同 ts 变（周期写回同值）不产生通知。
        let last = baseline(&[("d.t1", "1")]);
        let cur: HashMap<String, (String, serde_json::Value)> = [(
            "d.t1".to_string(),
            ("1".to_string(), json!([999, "lt", "1"])),
        )]
        .into_iter()
        .collect();
        let (changes, removed) = diff_scope(cur, &last);
        assert!(changes.is_empty() && removed.is_empty(), "值未变不推: {changes:?}");
    }

    #[test]
    fn diff_reports_removed_fields() {
        let last = baseline(&[("d.t1", "1"), ("d.t2", "2")]);
        let cur = scope(&[("d.t1", "1")]);
        let (changes, removed) = diff_scope(cur, &last);
        assert!(changes.is_empty());
        assert_eq!(removed, vec!["d.t2".to_string()]);
    }

    #[test]
    fn diff_empty_current_removes_all() {
        // 设备删除后其全部点位进 removed（e2e removed_on_device_delete 的纯函数面）。
        let last = baseline(&[("d.t1", "1")]);
        let (changes, removed) = diff_scope(HashMap::new(), &last);
        assert!(changes.is_empty());
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn filter_by_tag_keeps_matching_tail() {
        let m: serde_json::Map<String, serde_json::Value> = [
            ("devA.temp".to_string(), json!([0, "t", "1"])),
            ("devA.hum".to_string(), json!([0, "t", "2"])),
        ]
        .into_iter()
        .collect();
        let got = filter_by_tag(m, Some("temp"));
        assert_eq!(got.len(), 1);
        assert!(got.contains_key("devA.temp"));
        // None → 全保留。
        let m2: serde_json::Map<String, serde_json::Value> =
            [("a.b".to_string(), json!(1))].into_iter().collect();
        assert_eq!(filter_by_tag(m2, None).len(), 1);
    }
}
