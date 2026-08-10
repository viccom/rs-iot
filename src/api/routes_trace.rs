//! 报文 trace 开关 REST:per-instance 运行时激活/查询/关闭。JWT 保护(非 PUBLIC_PATHS)。
//!
//! `POST   /api/v1/inst/trace`  body `{inst_id, duration_secs?:60}` → 激活(实例须 Running,否则 404)。
//! `GET    /api/v1/inst/trace`  → 当前激活实例列表。
//! `DELETE /api/v1/inst/trace?inst_id=` → 关闭。
//!
//! trace 内容(收发字节 hex)经 tracing target `io_trace` 输出,不在此返回
//!(随子系统 3 ring buffer)。本端点只管开关;开关语义与实例生命周期解耦——
//! 激活一个已停实例无意义,故 `start` 仅放行 Running。

use crate::api::AppState;
use crate::plugin::instance::InstState;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

/// POST 请求体。
#[derive(Deserialize)]
pub struct TraceReq {
    /// 目标实例 ID。
    pub inst_id: String,
    /// 激活时长(秒,默认 60)。
    #[serde(default = "default_duration")]
    pub duration_secs: u64,
}

/// `duration_secs` 缺省 60s。
fn default_duration() -> u64 {
    60
}

/// DELETE 查询参数。
#[derive(Deserialize)]
pub struct IdQuery {
    /// 目标实例 ID。
    pub inst_id: String,
}

/// 当前 unix 毫秒(墙钟;SystemTime 失败兜底 0)。
fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `POST /api/v1/inst/trace`:激活实例报文 trace。
///
/// 实例须 Running(未运行/未注册 → 404,无意义开启保护)。
/// 激活后该实例后续收发即被 dump;返回 deadline_ms 便于客户端知道何时自动失效。
pub async fn start(State(st): State<AppState>, Json(req): Json<TraceReq>) -> Response {
    let running = matches!(st.mgr.status().get(&req.inst_id), Some(InstState::Running));
    if !running {
        return err(StatusCode::NOT_FOUND, "instance not running");
    }
    let deadline_ms = now_unix_ms() + (req.duration_secs as i64) * 1000;
    st.trace_registry.activate(&req.inst_id, deadline_ms);
    Json(serde_json::json!({"status":"ok","deadline_ms":deadline_ms})).into_response()
}

/// `DELETE /api/v1/inst/trace`:关闭实例 trace。
pub async fn stop(State(st): State<AppState>, Query(q): Query<IdQuery>) -> Response {
    st.trace_registry.deactivate(&q.inst_id);
    Json(serde_json::json!({"status":"ok"})).into_response()
}

/// `GET /api/v1/inst/trace`:当前激活实例列表(`active_ids` 已惰性过滤超时项)。
pub async fn list(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({"active": st.trace_registry.active_ids()}))
}

// ---- 局部类型/工具(对标 routes_command.rs / routes_sys.rs) ----
type StatusCode = axum::http::StatusCode;
type Response = axum::response::Response;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}
