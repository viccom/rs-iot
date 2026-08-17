//! 日志 ring buffer 快照 REST:取最近 N 条日志(结构化 JSON)。JWT 保护。
//!
//! `GET /api/v1/logs/recent?level=&target=&limit=` → `[LogEntry]`。
//! level=warn → error+warn(>= 给定级别);target=io_trace → 精确;limit 默认 200。

use crate::api::AppState;
use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

/// query 参数(均可选)。
#[derive(Deserialize, Default)]
pub struct RecentQuery {
    /// 过滤级别(>= 给定,如 "warn" → error+warn)。
    pub level: Option<String>,
    /// 过滤 target(精确,如 "io_trace")。
    pub target: Option<String>,
    /// 返回条数(默认 200)。
    pub limit: Option<usize>,
}

/// `GET /api/v1/logs/recent`:取最近日志(过滤 + 限数)。
pub async fn recent(
    State(st): State<AppState>,
    Query(q): Query<RecentQuery>,
) -> Json<Vec<crate::log_ringbuffer::LogEntry>> {
    let limit = q.limit.unwrap_or(200);
    let entries = st
        .log_ringbuffer
        .recent(q.level.as_deref(), q.target.as_deref(), limit);
    Json(entries)
}
