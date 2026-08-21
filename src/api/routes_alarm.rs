//! 活跃告警 REST 管理面（Q12 告警最小闭环，spec §4）。默认 JWT 保护。
//!
//! - `GET    /api/v1/alarms` → `[{key, level, source, message, ts, count}]`（按 ts 倒序）
//! - `DELETE /api/v1/alarms?key=` → 手动清除（运维兜底）+ 发 Clear 事件（SSE 前端收敛）
//!
//! 触发（fire）**无 REST 端点**——判定归脚本（T4 `ctx:fire_event`，spec §0）；
//! 审计 key 级不落 message 全文（防日志洪水，message 看 REST/SSE——spec §2）。

use crate::api::AppState;
use crate::db::alarms;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

/// `?key=<k>` 查询参数（DELETE）。
#[derive(Deserialize)]
pub struct KeyQuery {
    /// 告警键（脚本自定义，如 "dev1.temp_high"）。
    pub key: String,
}

/// `GET /api/v1/alarms` → 活跃清单（按 ts 倒序，空表 → `[]`）。
///
/// lux 故障 → 500；清单形状即 `db::alarms::AlarmItem`（前端面板同构）。
pub async fn list(State(st): State<AppState>) -> Response {
    match alarms::list(&st.lux).await {
        Ok(items) => Json(items).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `DELETE /api/v1/alarms?key=` → 手动清除（幂等：不存在仍 200 ok，spec §1.2）。
///
/// 条目存在才发 Clear 事件（level/message 沿用条目、source 原值、ts 取当下——
/// 前端收敛视图只需"该 key 已无告警"，不是历史细节）；清除恒执行（坏记录无条目
/// 信息可发事件，但仍要能 HDEL 掉）。lux 故障 → 500。
pub async fn delete(
    State(st): State<AppState>,
    auth: Option<axum::Extension<crate::api::auth::AuthedUser>>,
    Query(q): Query<KeyQuery>,
) -> Response {
    // 复审 P2-3：走宿主域服务（HDEL→publish 统一序；无条目幂等静默不发帧）。
    match alarms::clear_with_event(&st.lux, &st.event_bus, &q.key, false, "").await {
        Ok(_existed) => {
            // 审计（照 routes_secret 形态）：key 级不落 message 全文。
            tracing::info!(
                "[alarm] clear key={} by={}（via {}）",
                q.key,
                auth.as_ref()
                    .map(|a| a.user_id.as_str())
                    .unwrap_or("accesskey"),
                auth.as_ref().map(|a| a.via).unwrap_or("accesskey")
            );
            Json(serde_json::json!({"status": "ok", "key": q.key})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ---- 局部类型/工具（对标 routes_secret.rs） ----
type StatusCode = axum::http::StatusCode;
type Response = axum::response::Response;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}
