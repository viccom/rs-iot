//! 用户组 + 全局 accesskey（lux Hash `groups` / string `accesskey`）。

use crate::api::{hget, hgetall, AppState};
use axum::extract::State;
use axum::Json;
use serde::Deserialize;

pub async fn list(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(hgetall(&st.lux, "groups").await)
}

#[derive(Deserialize)]
pub struct MemberReq {
    pub user_id: String,
    pub group_id: String,
}

/// 把 user_id 加入 group_id（JSON 数组，去重）。
async fn mutate_group<F>(st: &AppState, group_id: &str, user_id: &str, f: F) -> serde_json::Value
where
    F: FnOnce(&mut Vec<String>, &str),
{
    let mut members: Vec<String> = match hget(&st.lux, "groups", group_id).await {
        Some(s) => serde_json::from_str(&s).unwrap_or_default(),
        None => Vec::new(),
    };
    f(&mut members, user_id);
    let json = serde_json::to_string(&members).unwrap_or_default();
    let _ = st.lux.execute("HSET", &["groups", group_id, &json]).await;
    serde_json::json!({"status": "ok"})
}

pub async fn add(
    State(st): State<AppState>,
    Json(req): Json<MemberReq>,
) -> Json<serde_json::Value> {
    Json(
        mutate_group(&st, &req.group_id, &req.user_id, |members, uid| {
            if !members.iter().any(|m| m == uid) {
                members.push(uid.to_string());
            }
        })
        .await,
    )
}

pub async fn remove(
    State(st): State<AppState>,
    Json(req): Json<MemberReq>,
) -> Json<serde_json::Value> {
    Json(
        mutate_group(&st, &req.group_id, &req.user_id, |members, uid| {
            members.retain(|m| m != uid);
        })
        .await,
    )
}

#[derive(Deserialize)]
pub struct AccessKeyReq {
    pub key: String,
}

pub async fn set_accesskey(
    State(st): State<AppState>,
    Json(req): Json<AccessKeyReq>,
) -> Json<serde_json::Value> {
    let _ = st.lux.execute("SET", &["accesskey", &req.key]).await;
    Json(serde_json::json!({"status": "ok"}))
}
