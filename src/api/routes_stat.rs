//! 统一实例指标端点（M4）：`GET /api/v1/inst/stats`，合并进程级 state + 插件级 health。
//!
//! 响应 `{inst_id, app_code, host_kind, state, health}`：
//! - `state`：进程级 InstState（InstanceManager::status，死活权威）
//! - `health`：lux stat:<inst> JSON（核心+extra），无则 null
//! - `app_code`/`host_kind`：来自 InstanceManager configs（实例分类）
//!
//! 不带 inst_id → 全实例数组。默认 JWT 保护。

use crate::api::AppState;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct InstQuery {
    pub inst_id: Option<String>,
}

/// `GET /api/v1/inst/stats?inst_id=<inst>` 单查 / 不带参全列。
pub async fn stats(State(st): State<AppState>, Query(q): Query<InstQuery>) -> Response {
    let states = st.mgr.status();
    match q.inst_id {
        Some(inst_id) => {
            let state = states.get(&inst_id).copied();
            let health = crate::db::stat::read(&st.lux, &inst_id)
                .await
                .ok()
                .flatten();
            let class = st.mgr.config_snapshot(&inst_id);
            let (app_code, host_kind) = class
                .as_ref()
                .map(|c| (c.app_code.to_string(), c.host_kind.as_str()))
                .unwrap_or((String::new(), ""));
            Json(serde_json::json!({
                "inst_id": inst_id,
                "app_code": app_code,
                "host_kind": host_kind,
                "state": state.map(state_str).unwrap_or_else(|| serde_json::Value::Null),
                "health": health,
            }))
            .into_response()
        }
        None => {
            let mut arr = Vec::with_capacity(states.len());
            for (inst_id, state) in states {
                let health = crate::db::stat::read(&st.lux, &inst_id)
                    .await
                    .ok()
                    .flatten();
                let class = st.mgr.config_snapshot(&inst_id);
                let (app_code, host_kind) = class
                    .as_ref()
                    .map(|c| (c.app_code.to_string(), c.host_kind.as_str()))
                    .unwrap_or((String::new(), ""));
                arr.push(serde_json::json!({
                    "inst_id": inst_id,
                    "app_code": app_code,
                    "host_kind": host_kind,
                    "state": state_str(state),
                    "health": health,
                }));
            }
            Json(serde_json::Value::Array(arr)).into_response()
        }
    }
}

/// InstState → JSON 值（与 InstState 的 serde lowercase 一致）。
fn state_str(s: crate::plugin::instance::InstState) -> serde_json::Value {
    serde_json::to_value(s).unwrap_or(serde_json::Value::Null)
}

// ---- 局部类型 ----
type Response = axum::response::Response;
