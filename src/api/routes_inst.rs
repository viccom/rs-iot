//! instance CRUD（lux Hash `instances`）。

use crate::api::{hget, hgetall, AppState};
use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct IdQuery {
    pub inst_id: String,
}

pub async fn list(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(hgetall(&st.lux, "instances").await)
}

pub async fn get(State(st): State<AppState>, Query(q): Query<IdQuery>) -> Json<serde_json::Value> {
    match hget(&st.lux, "instances", &q.inst_id).await {
        Some(s) => {
            let val = serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s));
            Json(serde_json::json!({ q.inst_id: val }))
        }
        None => Json(serde_json::json!({})),
    }
}

#[derive(Deserialize)]
pub struct SetReq {
    pub inst_id: String,
    pub object: serde_json::Value,
}

pub async fn set(State(st): State<AppState>, Json(req): Json<SetReq>) -> Json<serde_json::Value> {
    let mut obj = req.object;
    if let Some(m) = obj.as_object_mut() {
        m.insert("inst_id".into(), req.inst_id.clone().into());
    }
    let s = serde_json::to_string(&obj).unwrap_or_default();
    let _ = st
        .lux
        .execute("HSET", &["instances", &req.inst_id, &s])
        .await;
    Json(serde_json::json!({"status": "ok"}))
}

#[derive(Deserialize)]
pub struct DelReq {
    pub inst_id: String,
}

pub async fn delete(
    State(st): State<AppState>,
    Json(req): Json<DelReq>,
) -> Json<serde_json::Value> {
    let _ = st.lux.execute("HDEL", &["instances", &req.inst_id]).await;
    Json(serde_json::json!({"status": "deleted"}))
}
