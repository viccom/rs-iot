//! 日志 ring buffer(P1 子系统 3):tracing layer 截获最近 N 条日志到内存。
//!
//! spec: `docs/superpowers/specs/2026-08-10-log-ringbuffer-design.md`。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// 一条日志(结构化,tracing event 经 Visit 收集)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogEntry {
    /// 时间戳(ISO8601 / RFC3339)。
    pub ts: String,
    /// 级别("ERROR"/"WARN"/"INFO"/"DEBUG"/"TRACE")。
    pub level: String,
    /// target(模块路径,如 "io_trace"、"modbus_client")。
    pub target: String,
    /// 格式化可读文本(tracing message 字段经 display 包装)。
    pub message: String,
    /// 结构化字段(Visit 收集,值统一 String)。
    pub fields: HashMap<String, String>,
}

/// level 字符串 → 数字(ERROR=1 < WARN=2 < INFO=3 < DEBUG=4 < TRACE=5)。未知当 INFO(3)。
fn level_num(s: &str) -> u8 {
    match s.to_ascii_uppercase().as_str() {
        "ERROR" => 1,
        "WARN" => 2,
        "INFO" => 3,
        "DEBUG" => 4,
        "TRACE" => 5,
        _ => 3,
    }
}

/// 进程级日志 ring buffer(VecDeque + Mutex,容量 N)。
#[derive(Debug)]
pub struct LogRingBuffer {
    entries: Mutex<VecDeque<LogEntry>>,
    capacity: usize,
}

impl LogRingBuffer {
    /// 构造(容量 N)。
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    /// push 一条(满则 pop_front 淘汰最旧)。
    pub fn push(&self, entry: LogEntry) {
        let mut e = self.entries.lock().expect("ring buffer mutex poisoned");
        if e.len() >= self.capacity {
            e.pop_front();
        }
        e.push_back(entry);
    }

    /// 取最近条目(按 level>=/target 精确/limit 过滤;最近在前)。
    /// level=Some("warn") → 仅 error+warn;target=Some("io_trace") → 精确匹配;limit 上限数量。
    pub fn recent(&self, level: Option<&str>, target: Option<&str>, limit: usize) -> Vec<LogEntry> {
        let e = self.entries.lock().expect("ring buffer mutex poisoned");
        let threshold = level.map(level_num);
        e.iter()
            .rev()
            .filter(|en| threshold.map_or(true, |t| level_num(&en.level) <= t))
            .filter(|en| target.map_or(true, |tg| en.target == tg))
            .take(limit)
            .cloned()
            .collect()
    }
}

/// 当前 UTC 时间 RFC3339(ISO8601)。
///
/// 暂仅供测试引用；Task 3 的 `RingBufferLayer` 将在构造 `LogEntry` 时调用，
/// 届时移除下方的 `expect(dead_code)` 限定。
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "Task 3 RingBufferLayer 接入后启用")
)]
fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: &str, target: &str, message: &str) -> LogEntry {
        LogEntry {
            ts: "2026-08-10T00:00:00Z".into(),
            level: level.into(),
            target: target.into(),
            message: message.into(),
            fields: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn push_and_recent_basic() {
        let buf = LogRingBuffer::new(100);
        buf.push(entry("INFO", "app", "hello"));
        buf.push(entry("WARN", "app", "warn"));
        let r = buf.recent(None, None, 100);
        assert_eq!(r.len(), 2);
        // 最近在前(WARN 在前)
        assert_eq!(r[0].level, "WARN");
        assert_eq!(r[1].level, "INFO");
    }

    #[test]
    fn capacity_evicts_oldest() {
        let buf = LogRingBuffer::new(3);
        buf.push(entry("INFO", "a", "1"));
        buf.push(entry("INFO", "a", "2"));
        buf.push(entry("INFO", "a", "3"));
        buf.push(entry("INFO", "a", "4")); // 淘汰 "1"
        let r = buf.recent(None, None, 100);
        assert_eq!(r.len(), 3);
        assert_eq!(r[2].message, "2"); // 最旧剩 "2"(最近在前,最后是次旧)
        assert_eq!(r[0].message, "4");
    }

    #[test]
    fn recent_filters_level_ge() {
        let buf = LogRingBuffer::new(100);
        buf.push(entry("ERROR", "a", "e"));
        buf.push(entry("WARN", "a", "w"));
        buf.push(entry("INFO", "a", "i"));
        // level=warn → error+warn(INFO 数字大,滤掉)
        let r = buf.recent(Some("warn"), None, 100);
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|e| e.level == "ERROR" || e.level == "WARN"));
    }

    #[test]
    fn recent_filters_target_exact() {
        let buf = LogRingBuffer::new(100);
        buf.push(entry("INFO", "io_trace", "trace1"));
        buf.push(entry("INFO", "app", "normal"));
        let r = buf.recent(None, Some("io_trace"), 100);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].message, "trace1");
    }

    #[test]
    fn recent_respects_limit() {
        let buf = LogRingBuffer::new(100);
        for i in 0..10 {
            buf.push(entry("INFO", "a", &format!("m{i}")));
        }
        let r = buf.recent(None, None, 3);
        assert_eq!(r.len(), 3);
        // 最近 3 条(最近在前):m9 m8 m7
        assert_eq!(r[0].message, "m9");
        assert_eq!(r[2].message, "m7");
    }

    #[test]
    fn recent_empty_buffer() {
        let buf = LogRingBuffer::new(100);
        assert!(buf.recent(None, None, 100).is_empty());
    }

    #[test]
    fn now_iso8601_is_rfc3339() {
        let s = now_iso8601();
        // RFC3339 含 'T' 和 时区(Z 或 +00:00)
        assert!(s.contains('T'), "ISO8601 应含 T: {s}");
    }
}
