//! `selfMonitor`：rs-iot 自监控设备插件（devid=`rsiot`）。
//!
//! 自身识别/资源/统计标签周期上报 + 三可写标签（watchdog/note/report_period）；
//! 编排层无条件自启（替换旧 sim1 demo）。
//! 完整标签表见 docs/designs/2026-08-21-self-monitor-device-plan.md。
//!
//! 本模块 Task 2 只含观测结构与标签汇总纯函数；Source run 循环在 Task 3 补齐。

use std::collections::BTreeMap;

use crate::types::SampleValue;

/// 一次 tick 的原始观测（Source run 循环从 lux/metrics/静态信息组装；纯函数不耦合异步）。
#[derive(Debug, Default)]
pub(crate) struct SelfObservation {
    pub hwid: String,
    pub version: String,
    pub platform: String,
    pub uptime_s: u64,
    pub cpu: f32,
    pub mem_bytes: u64,
    pub collector_count: usize,
    pub device_count: usize,
    pub tag_count: usize,
    pub inst_running: usize,
    pub inst_degraded: usize,
    pub inst_failed: usize,
    pub alarm_active: u64,
    pub dbsize: i64,
    pub pipeline_processed: u64,
    pub hub_depth: u64,
    /// 可写标签当前值（None = 从未写过，不插键）。
    pub watchdog: Option<i64>,
    pub note: Option<String>,
}

/// 汇总为 rsiot 设备的标签表（BTreeMap——键有序，rtdata hash 与文档呈现稳定）。
/// 加标签=此处加一行 + 计划文档表格登记一行（扩展点）。
pub(crate) fn collect(obs: &SelfObservation) -> BTreeMap<String, SampleValue> {
    use SampleValue::{Bool, Float, Int, Str};
    let mut m = BTreeMap::new();
    m.insert("id".to_string(), Str(obs.hwid.clone()));
    m.insert("version".to_string(), Str(obs.version.clone()));
    m.insert("platform".to_string(), Str(obs.platform.clone()));
    m.insert("uptime_s".to_string(), Int(obs.uptime_s as i64));
    m.insert("cpu_percent".to_string(), Float(obs.cpu as f64));
    m.insert("mem_bytes".to_string(), Int(obs.mem_bytes as i64));
    // 自身健康 Bool：无降级、无失败业务实例即健康（平台侧最直观的一眼状态）。
    m.insert(
        "healthy".to_string(),
        Bool(obs.inst_degraded == 0 && obs.inst_failed == 0),
    );
    m.insert(
        "collector_count".to_string(),
        Int(obs.collector_count as i64),
    );
    m.insert("device_count".to_string(), Int(obs.device_count as i64));
    m.insert("tag_count".to_string(), Int(obs.tag_count as i64));
    m.insert("inst_running".to_string(), Int(obs.inst_running as i64));
    m.insert("inst_degraded".to_string(), Int(obs.inst_degraded as i64));
    m.insert("inst_failed".to_string(), Int(obs.inst_failed as i64));
    m.insert("alarm_active".to_string(), Int(obs.alarm_active as i64));
    m.insert("dbsize".to_string(), Int(obs.dbsize));
    m.insert(
        "pipeline_processed_count".to_string(),
        Int(obs.pipeline_processed as i64),
    );
    m.insert("hub_depth_last".to_string(), Int(obs.hub_depth as i64));
    if let Some(w) = obs.watchdog {
        m.insert("watchdog".to_string(), Int(w));
    }
    if let Some(n) = &obs.note {
        m.insert("note".to_string(), Str(n.clone()));
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 满观测样本（全部字段非默认值，避免实现靠 Default 蒙混；可写标签 None 见专项用例）。
    fn full_obs() -> SelfObservation {
        SelfObservation {
            hwid: "hw-001".to_string(),
            version: "0.1.2".to_string(),
            platform: "Windows 10 x86_64".to_string(),
            uptime_s: 1234,
            cpu: 12.5,
            mem_bytes: 45_678_901,
            collector_count: 3,
            device_count: 7,
            tag_count: 42,
            inst_running: 2,
            inst_degraded: 1,
            inst_failed: 0,
            alarm_active: 5,
            dbsize: 999,
            pipeline_processed: 8_675_309,
            hub_depth: 3,
            watchdog: None,
            note: None,
        }
    }

    #[test]
    fn readonly_tag_set_exact() {
        // 只读标签清单执行态：键集精确等于 17 名（多/漏都失败，加标签先改这里）。
        // BTreeMap keys 按字典序，期望 Vec 同序。
        let keys: Vec<_> = collect(&full_obs()).keys().cloned().collect();
        assert_eq!(
            keys,
            vec![
                "alarm_active",
                "collector_count",
                "cpu_percent",
                "dbsize",
                "device_count",
                "healthy",
                "hub_depth_last",
                "id",
                "inst_degraded",
                "inst_failed",
                "inst_running",
                "mem_bytes",
                "pipeline_processed_count",
                "platform",
                "tag_count",
                "uptime_s",
                "version",
            ]
        );
    }

    #[test]
    fn writable_none_omitted() {
        // watchdog/note 从未写过（None）→ 不插键，总键数=只读 17
        let m = collect(&full_obs());
        assert!(!m.contains_key("watchdog"));
        assert!(!m.contains_key("note"));
        assert_eq!(m.len(), 17);
    }

    #[test]
    fn writable_some_inserted_with_value() {
        // 写过才插键且值直传：watchdog Int、note Str（中文原样）
        let mut obs = full_obs();
        obs.watchdog = Some(7);
        obs.note = Some("机房A-1号网关".to_string());
        let m = collect(&obs);
        assert_eq!(m.get("watchdog"), Some(&SampleValue::Int(7)));
        assert_eq!(
            m.get("note"),
            Some(&SampleValue::Str("机房A-1号网关".to_string()))
        );
        assert_eq!(m.len(), 19);
    }

    #[test]
    fn value_types_covered() {
        // 四型覆盖：id/version/platform 是 Str，cpu_percent 是 Float（含小数），
        // healthy 是 Bool（降级/失败计数全零 → true），计数是 Int
        let m = collect(&full_obs());
        assert!(matches!(m["id"], SampleValue::Str(_)));
        assert!(matches!(m["version"], SampleValue::Str(_)));
        assert!(matches!(m["platform"], SampleValue::Str(_)));
        assert!(matches!(m["cpu_percent"], SampleValue::Float(_)));
        assert_eq!(
            m["healthy"],
            SampleValue::Bool(false),
            "full_obs inst_degraded=1 → 不健康"
        );
        for k in [
            "uptime_s",
            "mem_bytes",
            "collector_count",
            "device_count",
            "tag_count",
            "inst_running",
            "inst_degraded",
            "inst_failed",
            "alarm_active",
            "dbsize",
            "pipeline_processed_count",
            "hub_depth_last",
        ] {
            assert!(matches!(m[k], SampleValue::Int(_)), "{k} 应为 Int");
        }
    }

    #[test]
    fn numeric_values_passed_through() {
        // 数值直传不丢精度：Int 精确等值、Float 保留小数
        let m = collect(&full_obs());
        assert_eq!(m["uptime_s"], SampleValue::Int(1234));
        assert_eq!(m["mem_bytes"], SampleValue::Int(45_678_901));
        assert_eq!(m["alarm_active"], SampleValue::Int(5));
        assert_eq!(m["dbsize"], SampleValue::Int(999));
        assert_eq!(m["pipeline_processed_count"], SampleValue::Int(8_675_309));
        assert_eq!(m["hub_depth_last"], SampleValue::Int(3));
        assert_eq!(m["cpu_percent"], SampleValue::Float(12.5));
        assert_eq!(m["id"], SampleValue::Str("hw-001".to_string()));
    }

    #[test]
    fn empty_instance_stats_zero() {
        // 空实例口径：collector/device/tag/inst_* 全 0 → 对应键 Int(0)（而非缺键）
        let obs = SelfObservation {
            hwid: "hw-001".to_string(),
            version: "0.1.2".to_string(),
            platform: "Windows 10 x86_64".to_string(),
            ..SelfObservation::default()
        };
        let m = collect(&obs);
        for k in [
            "collector_count",
            "device_count",
            "tag_count",
            "inst_running",
            "inst_degraded",
            "inst_failed",
        ] {
            assert_eq!(m[k], SampleValue::Int(0), "{k} 空实例应为 Int(0)");
        }
        assert_eq!(m["healthy"], SampleValue::Bool(true), "全零 → 健康");
    }
}
