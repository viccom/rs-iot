//! SSE 实时推送端点（阶段一：日志 + 报文 + 实例状态）。
//!
//! `GET /api/v1/events?types=log,packet,state` → `text/event-stream`。
//! 鉴权走 cookie（后端 `/login` 已 Set-Cookie HttpOnly，EventSource 同源自动带）。
//!
//! 增量推送：每条事件一帧 `event: <kind>\ndata: <json>\n\n`，前端 EventSource
//! `addEventListener(kind)` 分发。无事件时 `KeepAlive` 周期发注释帧防代理超时。
//!
//! broadcast 语义：每 SSE 连接 `subscribe()` 得独立 Receiver，慢客户端缓冲满则
//! 收 `Lagged`（filter_map 丢帧继续，不终结流；丢失条数累入
//! `rs_iot_sse_lagged_total`，R6/C 阶段 T2——持续增长提示调大
//! `event_bus_capacity`）；无生产者时连接静默等待。

use crate::api::AppState;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use serde::Deserialize;
use std::convert::Infallible;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

/// query 参数：`?types=log,packet,state`（逗号分隔，缺省全推）。
#[derive(Deserialize, Default)]
pub struct EventsQuery {
    /// 过滤事件 kind（逗号分隔；缺省/空 = 全推）。
    pub types: Option<String>,
}

/// 解析 `?types=` 为 kind 集合（None = 全推）。
///
/// 纯函数，`pub` 供 tests/events_e2e.rs 单测引用。返回 `HashSet<String>`（owned，避生命周期）。
pub fn parse_types(raw: &str) -> Option<std::collections::HashSet<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let set: std::collections::HashSet<String> = trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if set.is_empty() {
        None
    } else {
        Some(set)
    }
}

/// `GET /api/v1/events`：SSE 流。
pub async fn events(State(st): State<AppState>, Query(q): Query<EventsQuery>) -> impl IntoResponse {
    let filter = q.types.as_deref().and_then(parse_types);
    let metrics = st.metrics.clone();
    let rx = st.event_bus.subscribe();
    // BroadcastStream: broadcast Receiver → Stream（Err=Lagged，丢帧继续）。
    // filter 丢 Lagged/不匹配 kind/序列化失败的项；map 转 SSE Event。
    // Lagged 按携带的丢失条数 n 累入进程指标（不再静默，R6 可观测）。
    let stream = BroadcastStream::new(rx).filter_map(move |res| match res {
        Err(BroadcastStreamRecvError::Lagged(n)) => {
            metrics.sse_lagged(n);
            None
        }
        Ok(payload) => {
            let kind = payload.kind();
            if filter.as_ref().is_some_and(|f| !f.contains(kind)) {
                return None;
            }
            Event::default()
                .event(kind)
                .json_data(payload)
                .ok()
                .map(Ok::<_, Infallible>)
        }
    });
    // 首帧即时性（2026-10-31）：开头垫一个注释帧——过滤型流静默时首个字节原本要等
    // KeepAlive 默认 15s 首拍，缓冲型代理（vite dev / 部分网关）会扣住响应头直到
    // 首个 body 字节，下游 15s 预算的 EventSource 直接超时（hmi-studio e2e 实测：
    // ttfb 恰 15.0039s）。注释帧对 EventSource 语义透明（规范要求忽略）。
    let stream = tokio_stream::iter(vec![Ok::<_, Infallible>(
        Event::default().comment("connected"),
    )])
    .chain(stream);
    Sse::new(stream).keep_alive(KeepAlive::default())
}
