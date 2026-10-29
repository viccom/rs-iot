//! 用户 CRUD + 登录/JWT/改密/验 token（lux Hash `users`/`passwords`）。

use crate::api::auth;
use crate::api::{hget, hgetall, AppState};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

/// 500 JSON 响应（审计 B1 写路径失败口径，对齐 routes_history 的 error 形态）。
fn internal_err(msg: &str) -> Response {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": msg})),
    )
        .into_response()
}

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

/// 写路径失败必须 5xx（审计 B1：原 `let _ =` 吞错谎报 ok——改密/绑定
/// 落库失败时客户端显示成功）；序列化失败同理，不再写空串。
pub async fn set(State(st): State<AppState>, Json(req): Json<SetReq>) -> Response {
    let Ok(s) = serde_json::to_string(&req.object) else {
        tracing::warn!("[routes_user] 用户对象序列化失败: {}", req.user_id);
        return internal_err("用户对象序列化失败");
    };
    if let Err(e) = st.lux.execute("HSET", &["users", &req.user_id, &s]).await {
        tracing::warn!("[routes_user] HSET users {} 失败: {e}", req.user_id);
        return internal_err("用户落库失败");
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

#[derive(Deserialize)]
pub struct DelReq {
    pub user_id: String,
}

pub async fn delete(State(st): State<AppState>, Json(req): Json<DelReq>) -> Response {
    // 两表清理：任一失败即 5xx（审计 B1）——半删状态可重试（delete 幂等）。
    if let Err(e) = st.lux.execute("HDEL", &["users", &req.user_id]).await {
        tracing::warn!("[routes_user] HDEL users {} 失败: {e}", req.user_id);
        return internal_err("用户删除失败");
    }
    if let Err(e) = st.lux.execute("HDEL", &["passwords", &req.user_id]).await {
        tracing::warn!("[routes_user] HDEL passwords {} 失败: {e}", req.user_id);
        return internal_err("密码记录删除失败");
    }
    Json(serde_json::json!({"status": "deleted"})).into_response()
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
            // 审计 B1：改密落库失败必须 5xx——原吞错返回 ok，用户以为改密
            // 成功，旧密码仍在生效（下次登录才发现）。
            if let Err(e) = st
                .lux
                .execute("HSET", &["passwords", &req.user_id, &h])
                .await
            {
                tracing::warn!("[routes_user] HSET passwords {} 失败: {e}", req.user_id);
                return internal_err("密码落库失败");
            }
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
    if let Ok(v) = session_cookie(&token).parse() {
        resp.headers_mut().insert(axum::http::header::SET_COOKIE, v);
    }
    resp
}

/// 会话 cookie 有效期 1 天（具名常量，审计 B6）。SameSite=Lax：统一会话后
/// cookie 是主凭证，Lax 挡跨站 POST 携带（CSRF 基线）；LAN http 不加 Secure。
const SESSION_COOKIE_MAX_AGE_SECS: u64 = 86_400;

fn session_cookie(token: &str) -> String {
    format!("token={token}; Path=/; HttpOnly; Max-Age={SESSION_COOKIE_MAX_AGE_SECS}; SameSite=Lax")
}

/// `POST /api/v1/user/session`：凭有效凭证（Bearer/cookie）补发会话 cookie。
///
/// 旧会话自愈（统一会话，2026-10-28）：部署前登录的浏览器只有 JWT 镜像、没有
/// cookie（旧 gateway 丢弃 Set-Cookie）——REST 走 Bearer 正常，SSE（EventSource
/// 只能靠同源 cookie）401 死循环。前端启动时带镜像 Bearer 调本端点静默补 cookie。
/// 须过 auth_middleware（非 PUBLIC）；顺带把 exp 续成新 24h（等价免密续登）。
pub async fn session(
    State(st): State<AppState>,
    authed: Option<axum::Extension<auth::AuthedUser>>,
) -> Response {
    let Some(axum::Extension(authed)) = authed else {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            "missing token".to_string(),
        )
            .into_response();
    };
    let token = match auth::encode_token(&authed.user_id, &st.jwt_secret) {
        Ok(t) => t,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("token error: {e}"),
            )
                .into_response()
        }
    };
    let mut resp =
        Json(serde_json::json!({"status": "ok", "user_id": authed.user_id})).into_response();
    if let Ok(v) = session_cookie(&token).parse() {
        resp.headers_mut().insert(axum::http::header::SET_COOKIE, v);
    }
    resp
}

pub async fn logout() -> Response {
    let mut resp = Json(serde_json::json!({"status": "ok"})).into_response();
    let expired = "token=; Path=/; HttpOnly; Max-Age=0; SameSite=Lax";
    if let Ok(v) = expired.parse() {
        resp.headers_mut().insert(axum::http::header::SET_COOKIE, v);
    }
    resp
}
