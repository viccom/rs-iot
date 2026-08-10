//! 报文 trace:per-instance 运行时开关 + 字节 dump(裸报文调试)。
//!
//! spec: `docs/superpowers/specs/2026-08-10-packet-trace-design.md`。
//!
//! 分层:dump 层统一(所有插件一个 `dump` 函数);拦截层按协议二选一——
//! modbus 用 `TraceIo<T>` 包装 transport(字节被 tokio-modbus 吞);lua 用 io_engine
//! direct dump(frame 已在手)。trace 关闭时 `dump` 快速路径仅一次 DashMap get + AtomicBool load。

use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// 字节流方向。
#[derive(Copy, Clone, Debug)]
pub enum Dir {
    /// 发送(TX)。
    Tx,
    /// 接收(RX)。
    Rx,
}

/// 单实例 trace 状态(并发访问)。
#[derive(Debug)]
pub struct TraceState {
    /// 是否激活(快速路径开关)。
    active: AtomicBool,
    /// 截止时刻(unix 毫秒,墙钟;惰性超时检查)。
    deadline_ms: AtomicI64,
}

impl TraceState {
    /// 构造(激活,deadline = now + duration)。
    fn new(deadline_ms: i64) -> Self {
        Self {
            active: AtomicBool::new(true),
            deadline_ms: AtomicI64::new(deadline_ms),
        }
    }

    /// 是否激活且未超时;超时则惰性置 active=false 并返回 false。
    pub fn check_alive(&self) -> bool {
        if !self.active.load(Ordering::Relaxed) {
            return false;
        }
        if now_unix_ms() > self.deadline_ms.load(Ordering::Relaxed) {
            self.active.store(false, Ordering::Relaxed);
            return false;
        }
        true
    }
}

/// per-instance trace 开关注册表。挂 `HostContext` + `AppState`(共享同一实例,Arc 内部)。
#[derive(Clone, Default, Debug)]
pub struct TraceRegistry {
    map: Arc<DashMap<String, Arc<TraceState>>>,
}

impl TraceRegistry {
    /// 构造空注册表。
    pub fn new() -> Self {
        Self {
            map: Arc::new(DashMap::new()),
        }
    }

    /// 激活实例 trace(deadline_ms = now + duration)。覆盖旧状态。
    pub fn activate(&self, inst_id: &str, deadline_ms: i64) {
        self.map
            .insert(inst_id.to_string(), Arc::new(TraceState::new(deadline_ms)));
    }

    /// 手动关闭。
    pub fn deactivate(&self, inst_id: &str) {
        self.map.remove(inst_id);
    }

    /// 取实例状态(None = 未激活)。
    pub fn get(&self, inst_id: &str) -> Option<Arc<TraceState>> {
        self.map.get(inst_id).map(|r| Arc::clone(r.value()))
    }

    /// 当前激活且未超时的实例(遍历时 check_alive 过滤超时项)。
    pub fn active_ids(&self) -> Vec<String> {
        self.map
            .iter()
            .filter(|r| r.value().check_alive())
            .map(|r| r.key().clone())
            .collect()
    }
}

/// dump 一段字节(查 registry → 未激活/超时则跳过;激活则 tracing 输出)。
///
/// 快速路径(未激活):一次 `DashMap::get` + `AtomicBool::load`,不格式化、不输出。
pub fn dump(registry: &TraceRegistry, inst_id: &str, dir: Dir, bytes: &[u8]) {
    let Some(state) = registry.get(inst_id) else {
        return;
    };
    if !state.check_alive() {
        return;
    }
    tracing::trace!(
        target: "io_trace",
        inst_id,
        dir = ?dir,
        hex = %hex(bytes),
    );
}

/// hex 工具:空格分隔大写字节序列(如 `"01 03 0A FF"`)。
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{b:02X}");
    }
    s
}

/// 当前 unix 毫秒(墙钟;SystemTime 失败兜底 0)。
fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn future_deadline_ms(extra_ms: i64) -> i64 {
        now_unix_ms() + extra_ms
    }

    #[test]
    fn registry_activate_get_deactivate() {
        let r = TraceRegistry::new();
        assert!(r.get("a").is_none());
        r.activate("a", future_deadline_ms(60_000));
        assert!(r.get("a").is_some());
        assert!(r.get("a").unwrap().check_alive());
        r.deactivate("a");
        assert!(r.get("a").is_none());
    }

    #[test]
    fn activate_overwrites_old_state() {
        let r = TraceRegistry::new();
        r.activate("a", future_deadline_ms(-1_000)); // 已过期
        r.activate("a", future_deadline_ms(60_000)); // 覆盖为未过期
        assert!(r.get("a").unwrap().check_alive(), "覆盖后应激活");
    }

    #[test]
    fn check_alive_expired_returns_false_and_lazily_clears() {
        let r = TraceRegistry::new();
        r.activate("a", future_deadline_ms(-1_000)); // 过去 deadline
        let st = r.get("a").unwrap();
        assert!(!st.check_alive(), "过期应 false");
        assert!(!st.check_alive(), "惰性置 active=false 后仍 false");
    }

    #[test]
    fn active_ids_filters_expired() {
        let r = TraceRegistry::new();
        r.activate("alive", future_deadline_ms(60_000));
        r.activate("expired", future_deadline_ms(-1_000));
        let mut ids = r.active_ids();
        ids.sort();
        assert_eq!(ids, vec!["alive".to_string()]);
    }

    #[test]
    fn hex_format_uppercase_space_separated() {
        assert_eq!(hex(&[0x01, 0x03, 0x0A, 0xFF]), "01 03 0A FF");
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x00]), "00");
    }

    #[test]
    fn dump_inactive_does_not_panic() {
        // 未激活:dump 快速返回,不 panic(无 subscriber 时不输出)
        let r = TraceRegistry::new();
        dump(&r, "missing", Dir::Tx, &[0x01, 0x02]);
        // 激活:有 subscriber 才输出,此处仅验不 panic
        r.activate("on", future_deadline_ms(60_000));
        dump(&r, "on", Dir::Rx, &[0x03, 0x04]);
    }

    #[test]
    fn dump_expired_state_skips() {
        let r = TraceRegistry::new();
        r.activate("expired", future_deadline_ms(-1_000));
        dump(&r, "expired", Dir::Tx, &[0x01]); // 过期:check_alive 清 active,不 panic
    }
}
