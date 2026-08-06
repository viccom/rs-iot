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
    // 先反序列化校验整体合法性（host_kind/app_code/app_type 等），失败直接返回错误——
    // 避免"set 成功存入非法配置、startinst 反序列化才报错"的延迟失败让用户困惑。
    let inst: crate::types::InstConfig = match serde_json::from_value(obj.clone()) {
        Ok(i) => i,
        Err(e) => {
            return Json(serde_json::json!({
                "status": "error",
                "msg": format!("invalid instance config: {e}")
            }));
        }
    };
    let s = serde_json::to_string(&obj).unwrap_or_default();
    let _ = st
        .lux
        .execute("HSET", &["instances", &req.inst_id, &s])
        .await;

    // 外部插件实例（host_kind=External）：签发 token，仅此次返回明文（管理员交付给外部进程）。
    // 注意：set 每次调 issue 会覆盖旧 token（旧 token 失效）；重复 set 即轮换。
    if inst.host_kind == crate::types::HostKind::External {
        match crate::db::ext_token::issue(&st.lux, &st.jwt_secret, &req.inst_id).await {
            Ok(token) => return Json(serde_json::json!({"status": "ok", "plugin_token": token})),
            Err(e) => tracing::warn!("[routes_inst] issue token for {} failed: {e}", req.inst_id),
        }
    }
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
    // 清除外部插件 token（实例没了，token 必失效——安全底线）。
    if let Err(e) = crate::db::ext_token::remove(&st.lux, &req.inst_id).await {
        tracing::warn!("[routes_inst] remove token for {} failed: {e}", req.inst_id);
    }
    Json(serde_json::json!({"status": "deleted"}))
}
