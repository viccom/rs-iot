//! JWT 签发/验证（HS256，24h）+ 全局鉴权中间件。
//!
//! 对标 Lite_IOT `bRedka/jwt_util.go` + `auth_middleware.go`。
//! 白名单（login/register/静态资源）放行；其余要 `Authorization`/Cookie token，
//! 或 `X-Access-Key` == lux `accesskey`。

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::api::{is_public, AppState};
use crate::error::TResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// subject = userId
    pub sub: String,
    /// 过期时间（Unix 秒）
    pub exp: usize,
}

pub fn encode_token(user_id: &str, secret: &str) -> TResult<String> {
    let exp = chrono::Local::now().timestamp() as usize + 24 * 3600;
    let claims = Claims {
        sub: user_id.to_string(),
        exp,
    };
    Ok(encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?)
}

pub fn verify_token(token: &str, secret: &str) -> Option<String> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .ok()?;
    Some(data.claims.sub)
}

/// axum 全局鉴权中间件。
pub async fn auth_middleware(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if is_public(&path) {
        return next.run(req).await;
    }

    // X-Access-Key 直通（对标 Lite_IOT globalAccessKey）
    if let Some(ak) = req
        .headers()
        .get("x-access-key")
        .and_then(|v| v.to_str().ok())
    {
        if let Ok(lux::EmbeddedValue::Simple(stored)) =
            st.lux.execute_value("GET", &["accesskey"]).await
        {
            if !stored.is_empty() && stored == ak {
                return next.run(req).await;
            }
        }
    }

    // Authorization header（"Bearer <jwt>" 或裸 token）或 Cookie token
    let token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s).to_string())
        .or_else(|| {
            req.headers()
                .get(axum::http::header::COOKIE)
                .and_then(|v| v.to_str().ok())
                .and_then(|c| c.split("token=").nth(1).and_then(|s| s.split(';').next()))
                .map(|s| s.to_string())
        });

    let token = match token {
        Some(t) => t,
        None => return (StatusCode::UNAUTHORIZED, "missing token").into_response(),
    };
    match verify_token(&token, &st.jwt_secret) {
        Some(_) => next.run(req).await,
        None => (StatusCode::UNAUTHORIZED, "invalid token").into_response(),
    }
}
