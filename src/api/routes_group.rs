//! 用户组 + 全局 accesskey（lux Hash `groups` / string `accesskey`）。

use crate::api::{hget, hgetall, AppState};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

/// 500 JSON 响应（审计 B1 写路径失败口径）。
fn internal_err(msg: &str) -> Response {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": msg})),
    )
        .into_response()
}

pub async fn list(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(hgetall(&st.lux, "groups").await)
}

#[derive(Deserialize)]
pub struct MemberReq {
    pub user_id: String,
    pub group_id: String,
}

/// 把 user_id 加入 group_id（JSON 数组，去重）。
async fn mutate_group<F>(st: &AppState, group_id: &str, user_id: &str, f: F) -> Response
where
    F: FnOnce(&mut Vec<String>, &str),
{
    let mut members: Vec<String> = match hget(&st.lux, "groups", group_id).await {
        Some(s) => serde_json::from_str(&s).unwrap_or_default(),
        None => Vec::new(),
    };
    f(&mut members, user_id);
    // 审计 B1：序列化/落库失败必须 5xx——原 unwrap_or_default+吞错谎报 ok
    //（失败时组数据不变，用户以为已加入/移出）。
    let Ok(json) = serde_json::to_string(&members) else {
        tracing::warn!("[routes_group] 组成员序列化失败: {group_id}");
        return internal_err("组成员序列化失败");
    };
    if let Err(e) = st.lux.execute("HSET", &["groups", group_id, &json]).await {
        tracing::warn!("[routes_group] HSET groups {group_id} 失败: {e}");
        return internal_err("组落库失败");
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

pub async fn add(State(st): State<AppState>, Json(req): Json<MemberReq>) -> Response {
    mutate_group(&st, &req.group_id, &req.user_id, |members, uid| {
        if !members.iter().any(|m| m == uid) {
            members.push(uid.to_string());
        }
    })
    .await
}

pub async fn remove(State(st): State<AppState>, Json(req): Json<MemberReq>) -> Response {
    mutate_group(&st, &req.group_id, &req.user_id, |members, uid| {
        members.retain(|m| m != uid);
    })
    .await
}

#[derive(Deserialize)]
pub struct AccessKeyReq {
    pub key: String,
}

pub async fn set_accesskey(State(st): State<AppState>, Json(req): Json<AccessKeyReq>) -> Response {
    // 审计 B1：accesskey 落库失败必须 5xx（原吞错谎报 ok）。
    if let Err(e) = st.lux.execute("SET", &["accesskey", &req.key]).await {
        tracing::warn!("[routes_group] SET accesskey 失败: {e}");
        return internal_err("accesskey 落库失败");
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}
