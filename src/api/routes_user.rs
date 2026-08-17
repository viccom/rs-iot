//! 用户 CRUD + 登录/JWT/改密/验 token（lux Hash `users`/`passwords`）。

use crate::api::auth;
use crate::api::{hget, hgetall, AppState};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct IdQuery {
    pub user_id: String,
}

pub async fn list(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(hgetall(&st.lux, "users").await)
}

pub async fn get(State(st): State<AppState>, Query(q): Query<IdQuery>) -> Json<serde_json::Value> {
    match hget(&st.lux, "users", &q.user_id).await {
        Some(s) => {
            let val = serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s));
            Json(serde_json::json!({ q.user_id: val }))
        }
        None => Json(serde_json::json!({})),
    }
}

#[derive(Deserialize)]
pub struct SetReq {
    pub user_id: String,
    pub object: serde_json::Value,
}

pub async fn set(State(st): State<AppState>, Json(req): Json<SetReq>) -> Json<serde_json::Value> {
    let s = serde_json::to_string(&req.object).unwrap_or_default();
    let _ = st.lux.execute("HSET", &["users", &req.user_id, &s]).await;
    Json(serde_json::json!({"status": "ok"}))
}

#[derive(Deserialize)]
pub struct DelReq {
    pub user_id: String,
}

pub async fn delete(
    State(st): State<AppState>,
    Json(req): Json<DelReq>,
) -> Json<serde_json::Value> {
    let _ = st.lux.execute("HDEL", &["users", &req.user_id]).await;
    let _ = st.lux.execute("HDEL", &["passwords", &req.user_id]).await;
    Json(serde_json::json!({"status": "deleted"}))
}

#[derive(Deserialize)]
pub struct VerifyReq {
    pub user_id: String,
    pub pass: String,
}

/// 校验密码（user_id + pass 是否匹配）。
async fn check_password(lux: &lux::EmbeddedClient, user_id: &str, pass: &str) -> bool {
    let hash = match hget(lux, "passwords", user_id).await {
        Some(h) => h,
        None => return false,
    };
    bcrypt::verify(pass, &hash).unwrap_or(false)
}

pub async fn verify(
    State(st): State<AppState>,
    Json(req): Json<VerifyReq>,
) -> Json<serde_json::Value> {
    let ok = check_password(&st.lux, &req.user_id, &req.pass).await;
    Json(serde_json::json!({"userId": req.user_id, "status": ok}))
}

#[derive(Deserialize)]
pub struct ChangePassReq {
    pub user_id: String,
    pub pass: String,
    pub new_pass: String,
}

pub async fn changepass(State(st): State<AppState>, Json(req): Json<ChangePassReq>) -> Response {
    if !check_password(&st.lux, &req.user_id, &req.pass).await {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            "旧密码不正确".to_string(),
        )
            .into_response();
    }
    match bcrypt::hash(&req.new_pass, bcrypt::DEFAULT_COST) {
        Ok(h) => {
            let _ = st
                .lux
                .execute("HSET", &["passwords", &req.user_id, &h])
                .await;
            Json(serde_json::json!({"status": "ok"})).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("hash error: {e}"),
        )
            .into_response(),
    }
}

pub async fn checktoken(
    State(st): State<AppState>,
    req_headers: axum::http::HeaderMap,
) -> Response {
    let token = req_headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    match auth::verify_token(token, &st.jwt_secret) {
        Some(uid) => Json(serde_json::json!({"status": "ok", "userId": uid})).into_response(),
        None => (
            axum::http::StatusCode::UNAUTHORIZED,
            "invalid token".to_string(),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct LoginReq {
    pub user_id: String,
    pub pass: String,
}

pub async fn login(State(st): State<AppState>, Json(req): Json<LoginReq>) -> Response {
    if !check_password(&st.lux, &req.user_id, &req.pass).await {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            "invalid user or password".to_string(),
        )
            .into_response();
    }
    let token = match auth::encode_token(&req.user_id, &st.jwt_secret) {
        Ok(t) => t,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("token error: {e}"),
            )
                .into_response()
        }
    };
    let user = hget(&st.lux, "users", &req.user_id)
        .await
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let mut resp = Json(serde_json::json!({
        "status": "ok",
        "token": token,
        "user": user,
    }))
    .into_response();
    let cookie = format!("token={token}; Path=/; HttpOnly; Max-Age=86400");
    if let Ok(v) = cookie.parse() {
        resp.headers_mut().insert(axum::http::header::SET_COOKIE, v);
    }
    resp
}

pub async fn logout() -> Response {
    let mut resp = Json(serde_json::json!({"status": "ok"})).into_response();
    let expired = "token=; Path=/; HttpOnly; Max-Age=0";
    if let Ok(v) = expired.parse() {
        resp.headers_mut().insert(axum::http::header::SET_COOKIE, v);
    }
    resp
}
