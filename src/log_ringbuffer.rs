//! 日志 ring buffer(P1 子系统 3):tracing layer 截获最近 N 条日志到内存。
//!
//! spec: `docs/superpowers/specs/2026-08-10-log-ringbuffer-design.md`。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};

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

/// tracing subscriber layer:截获 event → LogEntry → push LogRingBuffer。
///
/// 独立 EnvFilter("info,io_trace=trace")经 `.with_filter()` attach(见 init_tracing)。
#[derive(Debug)]
pub struct RingBufferLayer {
    /// 共享 ring buffer(Arc,init_tracing 建后注入 + AppState 同一实例)。
    pub buffer: std::sync::Arc<LogRingBuffer>,
    /// SSE 事件总线(阶段一):每条日志扇出。无 SSE 客户端时 publish 静默零开销。
    pub sender: crate::event_bus::EventBus,
}

impl<S: tracing::Subscriber> Layer<S> for RingBufferLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = EntryVisitor::default();
        event.record(&mut visitor);
        let entry = LogEntry {
            ts: crate::event_bus::now_iso8601(),
            level: meta.level().as_str().to_string(),
            target: meta.target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        };
        self.buffer.push(entry.clone());
        // 通讯报文（target=io_trace）走独立 packet 通道（io_trace::dump 直推
        // EventPayload::Packet），不进 log SSE 通道避免双推；ring buffer 仍保留
        // 供 GET /logs/recent?target=io_trace 补拉（SSE 断连兜底）。
        if entry.target != "io_trace" {
            self.sender.publish(crate::event_bus::EventPayload::Log {
                entry: std::sync::Arc::new(entry),
            });
        }
    }
}

/// Visit:message 字段(名 "message")→ message;str 字段 → fields(不带引号);其余 → fields(Debug)。
#[derive(Default)]
struct EntryVisitor {
    message: String,
    fields: HashMap<String, String>,
}

impl Visit for EntryVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        // str 字段直接存(不带 Debug 引号):inst_id/dir/hex 等。
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // message 字段经 display 包装(DisplayValue 的 Debug=Display),得可读文本(R1)。
        // 其余非 str 字段(bool/i64/u64/error)归这里。
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = s;
        } else {
            self.fields.insert(field.name().to_string(), s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tracing_subscriber::{layer::Layer, prelude::*, EnvFilter};

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
        // now_iso8601 已迁至 event_bus(crate 共用),此处验证其格式契约不变。
        let s = crate::event_bus::now_iso8601();
        // RFC3339 含 'T' 和 时区(Z 或 +00:00)
        assert!(s.contains('T'), "ISO8601 应含 T: {s}");
    }

    /// 用一个含 RingBufferLayer 的 dispatcher 驱动 tracing 宏,验 LogEntry 收集。
    fn with_layer_driver<F: FnOnce(&Arc<LogRingBuffer>)>(cap: usize, filter: &str, body: F) {
        let buffer = Arc::new(LogRingBuffer::new(cap));
        let layer = RingBufferLayer {
            buffer: buffer.clone(),
            sender: crate::event_bus::EventBus::dummy(),
        }
        .with_filter(EnvFilter::new(filter));
        let subscriber = tracing_subscriber::registry().with(layer);
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        tracing::dispatcher::with_default(&dispatch, || body(&buffer));
    }

    #[test]
    fn layer_captures_info_event() {
        with_layer_driver(100, "info", |buf| {
            tracing::info!("hello world");
            let r = buf.recent(None, None, 100);
            assert_eq!(r.len(), 1);
            assert_eq!(r[0].level, "INFO");
            assert_eq!(r[0].message, "hello world", "message 应可读不带引号(R1)");
        });
    }

    #[test]
    fn layer_captures_structured_fields() {
        with_layer_driver(100, "info", |buf| {
            tracing::info!(inst_id = "modbus-1", dir = "TX", "frame");
            let r = buf.recent(None, None, 100);
            assert_eq!(
                r[0].fields.get("inst_id").unwrap(),
                "modbus-1",
                "str 字段不带引号"
            );
            assert_eq!(r[0].fields.get("dir").unwrap(), "TX");
        });
    }

    #[test]
    fn layer_message_with_format_args() {
        with_layer_driver(100, "info", |buf| {
            tracing::info!("v={}", 42);
            let r = buf.recent(None, None, 100);
            assert_eq!(r[0].message, "v=42", "格式化 message 可读(R1)");
        });
    }

    #[test]
    fn layer_captures_io_trace_target() {
        // 报文 trace:trace level + target io_trace。filter 含 io_trace=trace 才进。
        with_layer_driver(100, "info,io_trace=trace", |buf| {
            tracing::trace!(target: "io_trace", inst_id = "lua-1", hex = "01 03", "dump");
            let r = buf.recent(None, Some("io_trace"), 100);
            assert_eq!(r.len(), 1);
            assert_eq!(r[0].target, "io_trace");
            assert_eq!(r[0].fields.get("hex").unwrap(), "01 03");
        });
    }

    #[test]
    fn layer_filter_excludes_debug_by_default() {
        // filter=info → debug 不进 buffer
        with_layer_driver(100, "info", |buf| {
            tracing::debug!("not captured");
            assert!(buf.recent(None, None, 100).is_empty());
        });
    }
}
