//! 报文 trace:per-instance 运行时开关 + 字节 dump(裸报文调试)。
//!
//! spec: `docs/superpowers/specs/2026-08-10-packet-trace-design.md`。
//!
//! 分层:dump 层统一(所有插件一个 `dump` 函数);拦截层按协议二选一——
//! modbus 用 `TraceIo<T>` 包装 transport(字节被 tokio-modbus 吞);lua 用 io_engine
//! direct dump(frame 已在手)。trace 关闭时 `dump` 快速路径仅一次 DashMap get + AtomicBool load。

use dashmap::DashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// 字节流方向。
#[derive(Copy, Clone, Debug)]
pub enum Dir {
    /// 发送(TX)。
    Tx,
    /// 接收(RX)。
    Rx,
}

impl Dir {
    /// 方向字符串(SSE `Packet.dir`:"tx"/"rx")。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tx => "tx",
            Self::Rx => "rx",
        }
    }
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
#[derive(Clone, Debug)]
pub struct TraceRegistry {
    map: Arc<DashMap<String, Arc<TraceState>>>,
    /// SSE 事件总线(阶段一):dump 激活时扇出 Packet。
    /// `new()` 自建 dummy(测试用,publish 静默),`with_sender()` 注入真 bus(生产用,
    /// 与 SSE endpoint / 其他生产者共享同一 bus)。dump 经此字段发布,5 个调用点零改。
    event_tx: crate::event_bus::EventBus,
}

impl Default for TraceRegistry {
    /// 默认 = 空 map + 孤立 event sender（与 `new()` 一致，测试用）。
    fn default() -> Self {
        Self::new()
    }
}

impl TraceRegistry {
    /// 构造空注册表 + 孤立 event sender(测试用,publish 静默)。
    pub fn new() -> Self {
        Self {
            map: Arc::new(DashMap::new()),
            event_tx: crate::event_bus::EventBus::dummy(),
        }
    }

    /// 构造并注入事件总线(生产:`run_app_with_shutdown` 建真 channel 后注入,
    /// 与 SSE endpoint / 其他生产者共享同一 bus)。
    pub fn with_sender(event_tx: crate::event_bus::EventBus) -> Self {
        Self {
            map: Arc::new(DashMap::new()),
            event_tx,
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
    let hex_str = hex(bytes);
    tracing::trace!(
        target: "io_trace",
        inst_id,
        dir = ?dir,
        hex = %hex_str,
    );
    // 扇出到 SSE 事件总线(阶段一):前端报文流实时滚动。复用 hex_str 避免重复格式化。
    registry
        .event_tx
        .publish(crate::event_bus::EventPayload::Packet {
            inst_id: inst_id.to_string(),
            dir: dir.as_str(),
            hex: hex_str,
            ts: crate::event_bus::now_iso8601(),
        });
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

/// transport 拦截层:包装底层 IO,在 `poll_read`/`poll_write` 拦截字节调 `dump`。
///
/// 仅服务 modbus(字节被 tokio-modbus 吞)。trace 关闭时 `dump` 快速路径跳过。
/// `derive(Debug)`:tcp 路径 `tcp::attach_slave` 要求 `T: Debug`(`TcpStream: Debug`);
/// RTU 路径 `rtu::attach_slave` 不要求(故 `TraceIo<UdpIo>` 不实现 Debug 也可走 rtu)。
#[derive(Debug)]
pub struct TraceIo<T> {
    /// 被包装的底层 transport(TcpStream / UdpIo / SerialStream)。
    inner: T,
    /// 实例 ID(dump 标识)。
    inst_id: String,
    /// 开关注册表。
    registry: TraceRegistry,
}

impl<T> TraceIo<T> {
    /// 构造:包一层 transport,绑定实例 ID + 开关注册表。
    pub fn new(inner: T, inst_id: String, registry: TraceRegistry) -> Self {
        Self {
            inner,
            inst_id,
            registry,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for TraceIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let inner = Pin::new(&mut this.inner);
        match inner.poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let filled = buf.filled();
                if !filled.is_empty() {
                    dump(&this.registry, &this.inst_id, Dir::Rx, filled);
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for TraceIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let inner = Pin::new(&mut this.inner);
        match inner.poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                // 仅 dump 本次实际写入的字节(Pending/部分写由下次 poll_write 续写时 dump)。
                dump(&this.registry, &this.inst_id, Dir::Tx, &buf[..n]);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

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

    #[tokio::test]
    async fn trace_io_forwards_write_bytes() {
        // TraceIo 包装 duplex 一端:写透传到另一端,字节不丢不改。
        let (client, mut server) = duplex(64);
        let registry = TraceRegistry::new();
        registry.activate("inst", now_unix_ms() + 60_000);
        let mut traced = TraceIo::new(client, "inst".into(), registry);
        traced.write_all(&[0x01, 0x03, 0xAB]).await.unwrap();
        let mut buf = [0u8; 3];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, &[0x01, 0x03, 0xAB]);
    }

    #[tokio::test]
    async fn trace_io_forwards_read_bytes() {
        // 读方向:对端写字节,TraceIo 透传读出。
        let (client, mut server) = duplex(64);
        let registry = TraceRegistry::new();
        registry.activate("inst", now_unix_ms() + 60_000);
        server.write_all(&[0x05, 0x06]).await.unwrap();
        let mut traced = TraceIo::new(client, "inst".into(), registry);
        let mut buf = [0u8; 2];
        traced.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, &[0x05, 0x06]);
    }

    #[tokio::test]
    async fn trace_io_inactive_still_forwards() {
        // trace 未激活:TraceIo 仍透传(dump 快速跳过),字节不丢。
        let (client, mut server) = duplex(64);
        let registry = TraceRegistry::new(); // 未 activate
        let mut traced = TraceIo::new(client, "inst".into(), registry);
        traced.write_all(&[0x07]).await.unwrap();
        let mut buf = [0u8; 1];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, &[0x07]);
    }

    #[test]
    fn trace_io_debug_impl() {
        // tcp::attach_slave 要求 T: Debug;TraceIo<TcpStream> 须实现 Debug。
        let (client, _server) = tokio::io::duplex(64);
        let registry = TraceRegistry::new();
        let t = TraceIo::new(client, "inst".into(), registry);
        let _s = format!("{t:?}");
    }
}
